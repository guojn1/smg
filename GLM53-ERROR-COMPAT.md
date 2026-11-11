# GLM-5.3 官网错误响应兼容

对应乐信 6945、6946、6947、6949、6950。默认关闭；专用 GLM-5.3/智谱兼容 router 启动前设置 `SMG_GLM53_OFFICIAL_ERROR_COMPAT=1`。取消此环境变量并重启恢复原行为。不要在混合模型通用入口直接开启：缺少 model 或鉴权失败的请求无法按模型划分范围。

- POST /v1/chat/completions：缺少 model、model=null、空对象 {} 返回 HTTP 200 和 `{"error":{"code":"500"}}`，不进入推理。
- POST /v1/responses：仅当 router 已配置 serving API Key 且校验失败时，缺少 Authorization 返回 HTTP 200 和 `{"code":1001,"msg":"Header中未收到Authorization参数，无法进行身份验证。","success":false}`；无效令牌返回 HTTP 200 和 `{"code":401,"msg":"令牌已过期或验证不正确","success":false}`。
- 开关只转换已有鉴权拒绝，不启用或绕过鉴权。未配置 API Key 的内部 router 不能判断外部客户 Token 是否有效，6949/6950 不会因此开始拒绝。有效凭据、管理接口及其他接口的既有行为不变。
- 若要在测试环境验收鉴权两项，需要另行配置 router serving API Key，并让可信网关携带对应凭据；不要擅自给现有内部 router 加鉴权。
- Chat 缺参检查位于鉴权/准入后、原始 HTTP 转发前，会先缓冲请求体；保留原有 payload 上限及读取超时。大请求需评估缓冲内存成本。关闭开关时不安装此检查。
- 有 model 的请求、非对象或格式错误 JSON 由原逻辑处理。正常请求字节不重写；空 function、坏 arguments、重复字段等继续由既有补丁处理。不统一转换其他 400/401，不修改 worker/Jinja。

验证：新增中间件 4 项隔离测试通过，直接引用当前 Rust 源文件，覆盖三个缺参场景、鉴权响应格式及作用域、历史原文保留、3 MiB 请求与配置上限。auth.rs 另补充鉴权矩阵测试，供完整 SMG 测试执行。隔离测试不代替完整构建/部署回归。

本次未打新 wheel、未提交或推送、未操作集群。之前的 wheel 不含本次补丁。
