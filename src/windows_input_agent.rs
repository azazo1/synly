use anyhow::Result;

pub fn request_elevation() -> Result<()> {
    synly::input::request_windows_input_elevation()
}
