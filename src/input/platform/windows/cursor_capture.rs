use crate::input::{DesktopLayout, DisplayRect, Point};

const BOGUS_MOTION_MARGIN: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum CapturePhase {
    Observing = 0,
    Arming = 1,
    Relaying = 2,
    Disarming = 3,
}

impl CapturePhase {
    pub(super) fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Arming,
            2 => Self::Relaying,
            3 => Self::Disarming,
            _ => Self::Observing,
        }
    }

    pub(super) fn suppresses_local_input(self) -> bool {
        !matches!(self, Self::Observing)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CursorMove {
    Observed { point: Point, dx: i32, dy: i32 },
    Relayed { dx: i32, dy: i32, bogus: bool },
    Ignored,
}

#[derive(Clone, Debug)]
pub(super) struct CursorCaptureTracker {
    layout: DesktopLayout,
    anchor: Point,
    last_point: Option<Point>,
    warping: bool,
}

impl CursorCaptureTracker {
    pub(super) fn new(layout: DesktopLayout, anchor: Point, initial_point: Option<Point>) -> Self {
        Self {
            layout,
            anchor,
            last_point: initial_point,
            warping: false,
        }
    }

    pub(super) fn anchor(&self) -> Point {
        self.anchor
    }

    pub(super) fn update_layout(&mut self, layout: DesktopLayout, anchor: Point) -> bool {
        let changed = self.anchor != anchor || self.layout != layout;
        self.layout = layout;
        self.anchor = anchor;
        changed
    }

    pub(super) fn begin_warp(&mut self, target: Point) {
        self.warping = true;
        self.last_point = Some(target);
    }

    pub(super) fn end_warp(&mut self, target: Point) {
        self.warping = false;
        self.last_point = Some(target);
    }

    pub(super) fn handle_move(&mut self, phase: CapturePhase, point: Point) -> CursorMove {
        if self.warping || matches!(phase, CapturePhase::Arming | CapturePhase::Disarming) {
            return CursorMove::Ignored;
        }

        if phase == CapturePhase::Relaying {
            let previous = self.last_point.unwrap_or(self.anchor);
            let dx = point.x.saturating_sub(previous.x);
            let dy = point.y.saturating_sub(previous.y);
            self.last_point = Some(self.anchor);
            if dx == 0 && dy == 0 {
                return CursorMove::Relayed { dx, dy, bogus: false };
            }
            let bogus = self.is_bogus_motion(dx, dy);
            return CursorMove::Relayed { dx, dy, bogus };
        }

        let previous = self.last_point.replace(point);
        let (dx, dy) = previous
            .map(|previous| {
                (
                    point.x.saturating_sub(previous.x),
                    point.y.saturating_sub(previous.y),
                )
            })
            .unwrap_or((0, 0));
        CursorMove::Observed { point, dx, dy }
    }

    fn is_bogus_motion(&self, dx: i32, dy: i32) -> bool {
        let bounds = virtual_bounds(&self.layout.displays);
        (dx < 0
            && dx
                .saturating_neg()
                .saturating_add(BOGUS_MOTION_MARGIN)
                > self.anchor.x - bounds.left)
            || (dx > 0
                && dx.saturating_add(BOGUS_MOTION_MARGIN) > bounds.right - self.anchor.x)
            || (dy < 0
                && dy
                    .saturating_neg()
                    .saturating_add(BOGUS_MOTION_MARGIN)
                    > self.anchor.y - bounds.top)
            || (dy > 0
                && dy.saturating_add(BOGUS_MOTION_MARGIN) > bounds.bottom - self.anchor.y)
    }
}

pub(super) fn select_capture_anchor(
    primary: Option<DisplayRect>,
    displays: &[DisplayRect],
) -> Option<Point> {
    primary.or_else(|| displays.first().copied()).map(|display| Point {
        x: display.x.saturating_add(display.width / 2),
        y: display.y.saturating_add(display.height / 2),
    })
}

fn virtual_bounds(displays: &[DisplayRect]) -> Rect {
    let Some(first) = displays.first().copied() else {
        return Rect { left: 0, top: 0, right: 1, bottom: 1 };
    };
    displays.iter().skip(1).fold(
        Rect {
            left: first.x,
            top: first.y,
            right: first.right(),
            bottom: first.bottom(),
        },
        |bounds, display| Rect {
            left: bounds.left.min(display.x),
            top: bounds.top.min(display.y),
            right: bounds.right.max(display.right()),
            bottom: bounds.bottom.max(display.bottom()),
        },
    )
}

#[derive(Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(test)]
mod tests {
    use super::{CapturePhase, CursorCaptureTracker, CursorMove, select_capture_anchor};
    use crate::input::{DesktopLayout, DisplayRect, Point};

    fn layout() -> DesktopLayout {
        DesktopLayout::new(vec![DisplayRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }])
        .unwrap()
    }

    #[test]
    fn first_observed_move_always_publishes_position() {
        let mut tracker = CursorCaptureTracker::new(layout(), Point { x: 50, y: 50 }, None);
        assert_eq!(
            tracker.handle_move(CapturePhase::Observing, Point { x: 0, y: 40 }),
            CursorMove::Observed {
                point: Point { x: 0, y: 40 },
                dx: 0,
                dy: 0,
            }
        );
    }

    #[test]
    fn relayed_move_uses_anchor_as_previous_position() {
        let mut tracker = CursorCaptureTracker::new(layout(), Point { x: 50, y: 50 }, None);
        assert_eq!(
            tracker.handle_move(CapturePhase::Relaying, Point { x: 57, y: 44 }),
            CursorMove::Relayed { dx: 7, dy: -6, bogus: false }
        );
    }

    #[test]
    fn warp_interval_discards_mouse_moves() {
        let mut tracker = CursorCaptureTracker::new(layout(), Point { x: 50, y: 50 }, None);
        tracker.begin_warp(Point { x: 50, y: 50 });
        assert_eq!(
            tracker.handle_move(CapturePhase::Relaying, Point { x: 3, y: 8 }),
            CursorMove::Ignored
        );
        tracker.end_warp(Point { x: 50, y: 50 });
    }

    #[test]
    fn large_motion_toward_virtual_edge_is_marked_bogus() {
        let mut tracker = CursorCaptureTracker::new(layout(), Point { x: 50, y: 50 }, None);
        assert_eq!(
            tracker.handle_move(CapturePhase::Relaying, Point { x: 0, y: 50 }),
            CursorMove::Relayed { dx: -50, dy: 0, bogus: true }
        );
    }

    #[test]
    fn primary_display_is_preferred_for_capture_anchor() {
        let displays = [
            DisplayRect { x: -1280, y: 200, width: 1280, height: 1024 },
            DisplayRect { x: 0, y: 0, width: 1920, height: 1080 },
        ];
        assert_eq!(
            select_capture_anchor(Some(displays[1]), &displays),
            Some(Point { x: 960, y: 540 })
        );
    }
}
