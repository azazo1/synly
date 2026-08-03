use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(crate) const SERVICE_PIPE_NAME: &str = r"\\.\pipe\synly-input-service";
pub(crate) const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SERVICE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ServiceRequest {
    SpawnInputAgent {
        command_pipe: String,
        event_pipe: String,
        token: String,
    },
}

impl ServiceRequest {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::SpawnInputAgent { .. } => "SpawnInputAgent",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ServiceResponse {
    Ok,
    Err(String),
}

pub(crate) fn read_request(pipe: &mut super::super::agent::pipe::NativePipe) -> Result<ServiceRequest> {
    super::super::agent::protocol::read_packet(pipe, SERVICE_REQUEST_TIMEOUT)
        .context("读取 Synly 输入服务请求失败")
}

pub(crate) fn write_request(
    pipe: &mut super::super::agent::pipe::NativePipe,
    request: &ServiceRequest,
) -> Result<()> {
    super::super::agent::protocol::write_packet(pipe, request, SERVICE_RESPONSE_TIMEOUT)
        .context("写入 Synly 输入服务请求失败")
}

pub(crate) fn write_response(
    pipe: &mut super::super::agent::pipe::NativePipe,
    response: &ServiceResponse,
) -> Result<()> {
    super::super::agent::protocol::write_packet(pipe, response, SERVICE_RESPONSE_TIMEOUT)
        .context("写入 Synly 输入服务响应失败")
}

pub(crate) fn read_response(pipe: &mut super::super::agent::pipe::NativePipe) -> Result<ServiceResponse> {
    super::super::agent::protocol::read_packet(pipe, SERVICE_RESPONSE_TIMEOUT)
        .context("读取 Synly 输入服务响应失败")
}
