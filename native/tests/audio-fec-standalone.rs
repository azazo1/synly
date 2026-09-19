// 用 rustc --test 直接运行生产 FEC 模块的测试, 不构建平台后端或依赖.
#![allow(dead_code)]

#[path = "../../src/audio"]
mod audio {
    pub mod error;
    pub mod protocol;
    mod fec;
}
