# Responses 协议兼容与现有功能补强

本计划按用户确认的范围实施：补齐协议兼容，接通已有功能，复杂能力保留数据并明确执行边界。

## 范围

1. 在项目内增加兼容适配层，不修改 Cargo 缓存 SDK。保留原始 JSON，已知事件继续严格校验；兼容附加事件及新输出类型；错误和缺少终止帧继续报错。
2. 保存 phase、refusal、消息边界、原始输出顺序及函数 namespace/caller。持久化兼容旧会话，Responses 请求恢复合法历史并避免重复；跨协议使用通用内容。
3. 接通拒绝文字和历史；正文与函数参数 done 事件用于补齐、核对和去重。函数调用走现有权限和执行链。
4. 服务端结果尽量复用现有展示，保存必要协议数据。音频播放、原生 Shell、Computer Use、程序执行、工具搜索调度、服务端 MCP 审批、后台任务管理保持未实现；收到需要执行的调用时明确提示支持边界。压缩项只保存和合法回传，不增加压缩调度。
5. 更新 README 支持边界与 AGENTS 维护注意事项。

## 分工

协议历史模块负责 sampling-types 的保存与转换；流兼容模块负责 sampler 的解码、消费与去重；主任务协调接口、集成、文档和验证。

## 验证

使用 tools/windows-protoc/test-release.ps1 定向测试 sampling-types 与 sampler：未知事件、已知事件损坏、未知嵌套输出、缺少终止帧、refusal、phase 多轮、多个消息与工具顺序、done 补齐/重复、复杂能力未执行、跨协议和旧会话兼容。

使用 tools/windows-protoc/build-release.ps1 正式 Release 构建；使用本地假上游验证实际客户端能够越过 metadata 完成工具调用并正确回传历史。仅使用 Release 构建，不自动提交计划或代码。
