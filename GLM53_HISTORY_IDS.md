# GLM5.3 历史工具 ID 与重复参数键兼容修复（2026-11-03）

## 依据和范围

对智谱官方 `/api/paas/v4/chat/completions` 与现有服务发送完全相同的五个最小请求，参数 `glm-5.3 / stream=false / reasoning_effort=low / max_tokens=32`。官方五个均 HTTP 200、正文 OK；现有服务只有正常对照成功，其余因调用 ID 缺失/为空或工具结果 ID 缺失返回400。请求和官方响应随测试 fixtures 保存，不包含密钥。

## 两侧必须配套

- SMG：`crates/protocols/src/chat_compat.rs`，在原有反序列化失败后的兼容分支，仅对 model 归一化后等于 glm53 的请求，将缺失调用 `id`、缺失结果 `tool_call_id` 填为空字符串以通过必填 String 类型。不会伪造唯一 ID 或推断调用对应关系；已有 ID 不变。共享请求/响应结构体不变，其他模型仍拒绝缺失字段。
- SGLang：`python/sglang/srt/entrypoints/openai/glm53_history.py`，在已有 GLM5.3 兼容分支接受缺失或空字符串历史 ID；有值的调用 ID 仍要求同批唯一。`serving_chat.py` 入口继续限定 model=glm53、reasoning parser=glm45、tool parser=glm47。
- 显式 null、非字符串、纯空白 ID 继续拒绝。重复非空调用 ID、空工具名、NaN/Infinity/数组/标量参数校验继续保留；历史参数字符串中的重复 JSON 键按下文补充兼容。
- 保留问题2/3的历史关联容错、原空桩清理、括号修复、输出过滤与生成约束。

## 配置和模板

不增加环境开关。保留既有 GLM5.3 parser 和历史兼容配置。缺失/空 ID 兼容不依赖 raw 模式；raw 坏参数历史仍须原先配套 Jinja。现有 `glm53_history.jinja` 已有 ID 不完整时按原顺序渲染工具结果的路径，此次不修改模板、模型推理参数或 CUDA Graph。

## 验证及上线边界

五个最小请求已加入两项目测试，并覆盖 stream=true/false 的入口处理。官方在线证据仅为非流式；本地 CPU 测试不代表新镜像的 GPU/生产验收。

两侧分别构建新镜像后，在已授权测试环境通过 router 复测上述五例，再回归 H1–H4 两轮恢复；同时确认其他模型、null/错误类型 ID、重复非空 ID、非有限值及非对象参数仍按原规则拒绝，并复测重复 old_text 请求成功。只升级 worker 无法修复 router 的 missing field 错误；只升级 router 仍会遇到旧 worker 的非空 ID 校验。

本次未部署、未改变集群配置。不要将五个最小请求的成功解读为恢复任意损坏历史或保证所有工具语义关联正确。

## 本次本地验证记录

- SGLang 专项测试 10 项、既有兼容测试含新增真实 ToolCall 回归 16 项，共26项通过。既有测试在本机用 AST 隔离 GPU CI 注册/基类导入，保留生产校验函数、真实 ToolCall 模型及原测试断言。
- SMG 新增三个真实 CompatibleChatJson 提取器测试，覆盖五个请求的流式/非流式入口和其他模型/错误类型保护；尚未执行通过：本机 Rust 1.98 缺 manifest，备用工具链离线解析 workspace 时缺 zeromq Git 依赖。不是已完成新镜像验收。
- 构建环境必须执行 `cargo test -p openai-protocol --features axum --lib` 后再出 router 镜像；不要因本机测试受阻改动生产依赖版本。

## 补充：历史 arguments 中重复 JSON 键

客户截图的 edit_file 历史参数含两次 old_text，即使值相同也触发 `historical arguments contain ambiguous or non-finite JSON`。2026-11-03 用保留两个 old_text 的最小请求验证：当前接口400，官方200/OK；删除重复键后双方均200/OK。完整请求和响应保存在两侧 duplicate-keys fixtures 下。本次不是客户完整请求回放。

修复只在 SGLang 既有 GLM5.3 兼容分支的历史参数校验允许重复键，仍拒绝 NaN、Infinity、非对象参数。校验阶段保留原始 arguments 字符串；后续沿用现有 orjson 参数预处理，按后一个键值覆盖前一个键值，再交给模板。没有新增预处理算法，不需要修改 Jinja。官方实测仅证明接受请求，尚未证明官方对冲突键值采用何种语义。

SMG 已把 arguments 当字符串转发，不需要为本问题新增生产代码；已加入口测试，确认普通路径及缺失 ID 的兼容重解析路径都原样保留重复键字符串。不要把此兼容扩大到 HTTP 请求外层的重复字段。括号修复辅助函数与模型输出防护不变。

新增测试覆盖同值/异值/嵌套重复键、真实预处理和模板内容、重复键与 NaN/Infinity 混合仍拒绝、其他模型保持严格。原 ID 修复、H1–H4 及生成输出防护回归继续通过。当前未提交、未 push、未部署；新镜像推理验收仍待完成。

## 2026-11-03 官网实测追加对齐（覆盖此前保留限制）

本次逐项对比官方与 models.yml 的 GLM-5.3 非流式入口：正常对照 200；官方接受 null/纯空白/数字/布尔 ID、重复非空调用 ID，拒绝对象/数组 ID。工具名称为空字符串或纯空白时官方 200，缺失或 null 时官方 400。历史 arguments **字符串**中包含数组、字符串/数字/布尔/null 标量、NaN、Infinity、-Infinity 时官方均 200/OK，当前外部入口均 400。另单独确认有限小数 ID 7.5 官方 200/OK。此处不涉及 HTTP JSON 外层的非有限数字或非字符串 arguments。

- SMG：在现有请求反序列化兼容分支，将缺失/null 历史 ID 归一为空字符串，数字/布尔 ID 归一为 JSON 标量文本；字符串保持原样，不生成唯一 ID。对象/数组仍拒绝。先处理原精确空占位再归一 ID，保留旧空占位清理和大请求体限制修复。名称空字符串、重复调用 ID 和 arguments 原文在原转发结构已可保留，不放宽共享响应类型，不放宽缺失/null 工具名。
- SGLang：仅 GLM5.3 请求模型入口增加数值/布尔历史 ID 字符串化，不改变共享 ToolCall 输出结构；glm45/glm47 历史兼容路径接受 null/空白及重复 ID、空字符串工具名。其他模型保持原规则。
- 非对象及非有限值 arguments 仅在既有 raw 模式透传为历史原文；先保留原缺左/右花括号修复，再交给已有非严格预处理及配套模板渲染，不修改 Jinja、不把坏参数替换成空对象、不执行工具。
- 配置沿用 SGLANG_GLM53_COMPAT=1、SGLANG_GLM53_HISTORY_COMPAT=raw、--reasoning-parser glm45、--tool-call-parser glm47 及带 GLM53_HISTORY_RAW_FALLBACK_V1 标记的配套模板。off/repair 不增加非对象/非有限参数透传。
- 输出空 function 防护、输出参数对象检查、共享括号修复函数保持原样。CPU 回归 11 项专项 + 16 项原兼容测试通过，覆盖官网请求的流式/非流式输入处理、真实 ToolCall 类型、配套模板、原 DOCX/H1-H4 和输出防护；不是 GPU 推理验收。
- SMG 已增加真实 CompatibleChatJson 入口回归代码，本次未在 Windows 编译或运行 Rust 测试。需要在构建环境执行协议测试后才能认定 router 通过。
- 外部入口的数字/布尔 ID 在 Go 网关就被拒绝；空字符串工具名实测返回下游 missing name，尚未定位哪层丢失了字段。因此只改 SMG/worker 不能承诺外部 URL 全部通过，不能把缺失工具名一律补空来掩盖问题。
- 本次未提交、未 push、未部署。200 上此前启动的构建未被覆盖，**其 wheel 不包含本节新改动**。

证据：D:/workspace1/dynamo/glm53-official-history-id-20261103-135544、glm53-modelsyml-history-id-20261103-135820、glm53-history-validation-compare-20261103-140107。SGLang 仓库内同步保留本轮最小请求及结果摘要于 test/registered/unit/fixtures/glm53_history/official-alignment-20261103。
