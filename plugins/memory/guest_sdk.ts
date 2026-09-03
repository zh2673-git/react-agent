// guest_sdk：指向内核仓库 bindings/typescript 的唯一桥接 shim。
// 默认布局下从 plugins/memory/ 上溯 3 级到 "tool/"，再进 agent-kernel。
// 若内核 checkout 位于别处，仅需修改本文件的相对路径。
// 裸说明符 @grpc/grpc-js / @grpc/proto-loader 从内核 index.ts 自身目录向上解析，
// 因此需在内核仓 bindings/typescript 下执行过一次 npm install。
export { serve, type Plugin, type PluginManifest } from "../../../agent-kernel/bindings/typescript/src/index.ts";
