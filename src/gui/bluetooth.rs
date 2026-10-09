//! 蓝牙设备逐行更新, 保持列表模型和已有按钮实例稳定.
use super::{AppWindow, BluetoothRow};
use crate::core::{AppLifecycle, AppSnapshot};
use slint::{Model, ModelRc, VecModel};

pub(super) fn apply(window: &AppWindow, snapshot: &AppSnapshot) {
    let can_connect = matches!(snapshot.lifecycle, AppLifecycle::Idle | AppLifecycle::Error);
    if window.get_bluetooth_can_connect() != can_connect {
        tracing::info!(can_connect, scanning = snapshot.bluetooth_scanning, lifecycle = ?snapshot.lifecycle, "蓝牙连接操作状态已更新");
    }
    window.set_bluetooth_can_connect(can_connect);
    window.set_bluetooth_status(snapshot.bluetooth_status.clone().into());
    window.set_bluetooth_scanning(snapshot.bluetooth_scanning);
    window.set_bluetooth_can_refresh(snapshot.sessions.is_empty() && !snapshot.bluetooth_scanning && !matches!(snapshot.lifecycle, AppLifecycle::Connecting | AppLifecycle::Pairing | AppLifecycle::Reconfiguring | AppLifecycle::Stopping));
    let rows = snapshot.bluetooth_peers.iter().map(|peer| BluetoothRow {
        address: peer.address.clone().into(), title: peer.display_name.clone().into(),
        subtitle: format!("{} | {}", peer.address, peer.detail).into(), connectable: peer.connectable,
    }).collect::<Vec<_>>();
    let current = window.get_bluetooth_peers();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<BluetoothRow>>() {
        for (index, row) in rows.iter().enumerate() {
            let previous = model.row_data(index);
            if row.connectable && !previous.as_ref().is_some_and(|previous| previous.connectable) {
                tracing::info!(address = %row.address, scanning = snapshot.bluetooth_scanning, can_connect, lifecycle = ?snapshot.lifecycle, "蓝牙服务结果已更新到 UI 连接按钮");
            }
            match previous {
                Some(previous) if previous.address == row.address && previous.title == row.title && previous.subtitle == row.subtitle && previous.connectable == row.connectable => {},
                Some(_) => model.set_row_data(index, row.clone()),
                None => model.push(row.clone()),
            }
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
    } else {
        window.set_bluetooth_peers(ModelRc::new(VecModel::from(rows)));
    }
}
