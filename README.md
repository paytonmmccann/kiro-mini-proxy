# kiro-mini-proxy

一个**无状态**的 Anthropic Claude API 兼容代理服务：客户端通过 HTTP header 传入 Kiro 凭证，服务端拿到即用、原样回转响应，**自身不维护凭据池、不做负载均衡与故障转移、不持久化**，凭据管理完全交由调用方负责。

> 本项目改造自 [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs)，在其协议转换与流式解码能力基础上精简为无状态透传版本。

> **免责声明**：本项目仅供研究使用，Use at your own risk。与 AWS / KIRO / Anthropic / Claude 等官方无关。

---

## 特性

- **无状态透传**：凭证随请求 header 传入，服务端不存储、不回写、不缓存
- **Anthropic API 兼容**：支持 `/v1/messages`（流式 + 非流式）
- **三种认证**：Social / IdC（含 builder-id、iam）/ API Key
- **按需刷新 Token**：传入未过期 `accessToken` 直接用；只有 `refreshToken` 时现场刷新（不落地）
- **每请求可覆盖**：设备指纹、代理、Region 均可通过独立 header 覆盖
- **能力保留**：Thinking 模式、工具调用（tool use）、WebSearch、Claude Code 缓冲端点

---

## 编译

前置：Rust 工具链（`edition 2024`，需 **rustc 1.85+**），无需构建前端。

```bash
cargo build --release
# 产物：target/release/kiro-mini-proxy
```

**交叉编译**（支持 Windows / Linux glibc+musl / macOS 的 x64 与 arm64）：

```bash
rustup target add x86_64-unknown-linux-musl
# musl 目标用 rustls，避免 OpenSSL 交叉编译依赖
cargo build --release --target x86_64-unknown-linux-musl --no-default-features
```

> `.github/workflows/` 下已提供多平台二进制与多架构 Docker 镜像的自动构建（推送 `v*` tag 或手动触发）。

---

## Docker

```bash
# 构建镜像（当前架构）；多架构用 docker buildx --platform linux/amd64,linux/arm64
docker build -t kiro-mini-proxy .

# 启动（先准备 ./config/config.json）
IMAGE_OWNER=your-owner IMAGE_TAG=latest docker compose up -d
```

镜像基于 `alpine` + `rustls`（`--no-default-features`），无需 OpenSSL；配置挂载于 `/app/config/config.json`，监听 `8991`。

---

## 配置

复制 `config.example.json` 为 `config.json` 并按需修改：

| 字段 | 类型 | 默认值 | 描述 |
|------|------|--------|------|
| `host` | string | `127.0.0.1` | 服务监听地址 |
| `port` | number | `8080` | 服务监听端口（示例配置为 `8991`） |
| `apiKey` | string | - | **服务自身认证密钥**（客户端调用本服务时用，必配） |
| `region` | string | `us-east-1` | 默认 AWS 区域（可被 header 覆盖） |
| `authRegion` | string | - | 默认 Auth Region（Token 刷新），未配置回退 `region` |
| `apiRegion` | string | - | 默认 API Region（API 请求），未配置回退 `region` |
| `kiroVersion` | string | `0.11.107` | 伪造的 Kiro 版本号 |
| `systemVersion` | string | 随机 | 伪造的系统版本标识 |
| `nodeVersion` | string | `22.22.0` | 伪造的 Node.js 版本标识 |
| `machineId` | string | - | 全局默认机器码（64 位十六进制，可被 header 覆盖） |
| `tlsBackend` | string | `rustls` | TLS 后端：`rustls` 或 `native-tls` |
| `proxyUrl` | string | - | 全局默认代理（可被 header 覆盖） |
| `proxyUsername` / `proxyPassword` | string | - | 全局默认代理认证 |
| `countTokensApiUrl` / `countTokensApiKey` | string | - | 外部 count_tokens API（可选） |
| `countTokensAuthType` | string | `x-api-key` | 外部 API 认证类型：`x-api-key` 或 `bearer` |
| `extractThinking` | boolean | `true` | 非流式响应是否把 `<thinking>` 解析为独立内容块 |

最小配置：

```json
{
  "host": "127.0.0.1",
  "port": 8991,
  "apiKey": "sk-mini-your-secret-key",
  "region": "us-east-1"
}
```

---

## 启动

```bash
./target/release/kiro-mini-proxy -c config.json
```

| 参数 | 描述 |
|------|------|
| `-c, --config <PATH>` | 配置文件路径（默认 `config.json`） |

日志级别用环境变量控制：`RUST_LOG=debug ./target/release/kiro-mini-proxy -c config.json`

---

## 认证与 Header

一次请求涉及**两层认证**，互不相同：

1. **服务自身认证**（保护本服务不被滥用）：`x-api-key` 或 `Authorization: Bearer`，值为 `config.json` 里的 `apiKey`。
2. **上游 Kiro 凭证**（本服务拿去请求上游）：通过 `x-kiro-credentials` 等 header 传入。

### 凭证 header

| Header | 必填 | 描述 |
|--------|------|------|
| `x-kiro-credentials` | 是 | **base64 编码**的完整 credentials JSON |
| `x-kiro-machine-id` | 否 | 覆盖设备指纹（64 位十六进制） |
| `x-kiro-proxy-url` | 否 | 覆盖代理地址；特殊值 `direct` 表示强制直连 |
| `x-kiro-proxy-username` / `x-kiro-proxy-password` | 否 | 覆盖代理认证 |
| `x-kiro-region` | 否 | 覆盖 Region（兼容字段） |
| `x-kiro-auth-region` | 否 | 覆盖 Auth Region（Token 刷新） |
| `x-kiro-api-region` | 否 | 覆盖 API Region（API 请求） |

> **base64 必须单行无换行**：macOS 用 `base64`，Linux 用 `base64 -w0`。

### credentials JSON 字段

`x-kiro-credentials` 解码后是一段 JSON：

| 字段 | 类型 | 描述 |
|------|------|------|
| `accessToken` | string | OAuth 访问令牌（可选；未过期时直接使用，跳过刷新） |
| `refreshToken` | string | OAuth 刷新令牌（无 accessToken 时用它现场刷新） |
| `expiresAt` | string | accessToken 过期时间（RFC3339），用于判断是否需要刷新 |
| `authMethod` | string | `social` / `idc` / `api_key`（`builder-id`、`iam` 会归一为 `idc`） |
| `clientId` / `clientSecret` | string | IdC 认证必填 |
| `profileArn` | string | AWS Profile ARN（部分企业 IdC 必填） |
| `kiroApiKey` | string | API Key 凭证（`ksk_...`），直接作为 Bearer Token |
| `machineId` | string | 凭据级机器码（可被 `x-kiro-machine-id` 覆盖） |
| `region` / `authRegion` / `apiRegion` | string | 凭据级 Region（可被对应 header 覆盖） |
| `proxyUrl` / `proxyUsername` / `proxyPassword` | string | 凭据级代理（可被对应 header 覆盖） |

### 优先级与 Token 解析

指纹 / 代理 / Region 的取值优先级：

```
独立 header  >  credentials JSON 内字段  >  config.json 默认
```

服务端按以下顺序决定请求上游用的 Bearer Token：

1. API Key 凭据（有 `kiroApiKey` 或 `authMethod=api_key`）→ 直接用 `kiroApiKey`
2. 有未过期的 `accessToken`（依据 `expiresAt`，提前 5 分钟视为过期）→ 直接用
3. 否则用 `refreshToken` **现场刷新**（无状态，结果不落地、不回传）

> 若既无有效 `accessToken` 又无 `refreshToken`，返回 400。

---

## 使用示例

三种凭证形态（`x-kiro-credentials` 的 JSON 内容）：

```jsonc
// Social
{"refreshToken":"...","authMethod":"social"}
// IdC（企业版需 profileArn）
{"refreshToken":"...","authMethod":"idc","clientId":"...","clientSecret":"...","profileArn":"arn:aws:codewhisperer:..."}
// API Key
{"kiroApiKey":"ksk_...","authMethod":"api_key"}
```

完整请求（Social + 流式）：

```bash
CRED=$(printf '%s' '{"refreshToken":"你的refreshToken","authMethod":"social"}' | base64)

curl http://127.0.0.1:8991/v1/messages \
  -H "x-api-key: sk-mini-your-secret-key" \
  -H "x-kiro-credentials: $CRED" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4-5-20250929",
    "max_tokens": 1024,
    "stream": true,
    "messages": [{"role": "user", "content": "Hello, Claude!"}]
  }'
```

覆盖设备指纹 / 代理 / Region（追加 header 即可）：

```bash
curl http://127.0.0.1:8991/v1/messages \
  -H "x-api-key: sk-mini-your-secret-key" \
  -H "x-kiro-credentials: $CRED" \
  -H "x-kiro-machine-id: a1b2c3d4e5f6...（64位十六进制）" \
  -H "x-kiro-api-region: us-west-2" \
  -H "x-kiro-proxy-url: socks5://127.0.0.1:1080" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-5-20250929","max_tokens":1024,"messages":[{"role":"user","content":"Hi"}]}'
```

> 强制直连（忽略全局代理）：`-H "x-kiro-proxy-url: direct"`

Thinking / 工具调用与标准 Anthropic API 一致，直接在请求体带 `thinking` 或 `tools` 字段。

---

## API 端点

| 端点 | 方法 | 描述 |
|------|------|------|
| `/v1/models` | GET | 获取可用模型列表 |
| `/v1/messages` | POST | 创建消息（实时流式 / 非流式） |
| `/v1/messages/count_tokens` | POST | 估算 Token 数量 |
| `/cc/v1/messages` | POST | Claude Code 兼容端点（缓冲模式，`input_tokens` 更准确） |
| `/cc/v1/messages/count_tokens` | POST | 估算 Token 数量（同 `/v1`） |

> `/v1/messages` 实时流式返回、`input_tokens` 为估算值；`/cc/v1/messages` 等待上游流完成后用 `contextUsageEvent` 校正 `input_tokens` 再一次性返回，等待期间每 25 秒发送 `ping` 保活。

---

## 模型映射

请求体的 `model` 按下表关键字匹配映射为 Kiro 内部模型 ID：

| Anthropic 模型（含关键字） | Kiro 模型 |
|---------------------------|-----------|
| `fable` | `claude-fable-5.1` |
| `sonnet-5` | `claude-sonnet-5` |
| `sonnet` + `4.6`/`4-6` | `claude-sonnet-4.6` |
| `sonnet` + `4.5`/`4-5` | `claude-sonnet-4.5` |
| `opus-5.5`/`opus-5-5` | `claude-opus-5.5` |
| `opus-5` | `claude-opus-5` |
| `opus` + `4.8`/`4.7`/`4.6`/`4.5` | 对应 `claude-opus-4.x` |
| `haiku` | `claude-haiku-4.5` |

---

## 注意事项

1. **单端点**：当前固定使用 `ide` 端点，credentials 中的 `endpoint` 字段会被忽略。
2. **无状态**：刷新后的 Token 不回传、不落地；若需复用，请在客户端自行缓存并在下次请求带上新的 `accessToken` + `expiresAt`。
3. **重试**：仅对上游瞬态错误（网络错误 / 408 / 429 / 5xx）做同凭证指数退避重试（最多 3 次）；400/401/402/403 等直接返回。
4. **凭证安全**：`x-kiro-credentials` 含敏感信息，请仅在可信链路（HTTPS / 内网）中使用。
5. **base64 换行**：务必使用单行 base64，含换行会导致解码失败。
6. **TLS**：如遇代理证书或刷新失败问题，可在 `config.json` 将 `tlsBackend` 切为 `native-tls`（需使用含 native-tls 的构建）。

---

## License

MIT
