//! react-agent-host 库目标：装配组件（config/frontend/manifests/spawn）供 e2e 测试复用；
//! 宿主二进制（main.rs）是本库的薄入口。

pub mod config;
pub mod frontend;
pub mod manifests;
pub mod spawn;
