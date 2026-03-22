# Core Review — 核心链路实现审查

本文件只保留审查来源和实施规划入口，不再重复维护逐条问题正文。

- 审查范围：`Client::execute` 到 transport 层的完整请求链路，覆盖连接管理、协议绑定、策略执行与异步安全性。
- 审查基线：main 分支 commit `9abcfc1`。
- 实施规划：[`docs/plans/core-review-plan-spec.md`](plans/core-review-plan-spec.md)。

当前实现与验证结果应以代码、测试和对应设计文档为准：

- [`README.md`](../README.md)
- [`docs/DESIGN.md`](DESIGN.md)
