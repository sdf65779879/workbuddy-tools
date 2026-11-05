# WorkBuddy 鑫杭加速

> 2026-11-05：全项目 UI 已重新设计，原概览页改为用量统计。新版功能、统计口径与图标资源见 [界面更新说明](docs/UI_REDESIGN.md)。

一个本地的 **WorkBuddy 账号保险柜 + OpenAI 兼容网关**：管理多个 WorkBuddy 账号，一键切换当前账号、批量签到/保活、跑成长任务，并把账号额度变成 OpenAI `/v1` 接口（多号轮转）供第三方客户端使用。

技术栈与 [cockpit-tools](https://github.com/jlcodes99/cockpit-tools) 保持一致：

- **Tauri 2**（Rust 后端 + 本地网关引擎）
- **React 19 + TypeScript + Vite 7**
- **Tailwind CSS 3 + daisyUI 5**
- **Zustand 5** 状态管理
- **i18next**（中 / 英）
- Rust 网关：**axum + tokio + reqwest**

## 主要功能

- **账号保险柜**：卡片式管理多个账号（编号徽章 + 积分/token/成长任务/登录IP 统计块），一键「切换」当前使用的账号、签到、保活（续 token）、改名、分享、删除；卡片式导入账号（粘贴 token / 从日志提取 / 本机导入当前登录态）。
- **运维·签到积分**：一键全号签到、刷新全部积分、保活全部（逐个续 token），支持后台自动保活/自动签到。
- **成长任务**：一键全做（成长任务 + 每日福利）、只刷成长任务、只领每日福利，自动领奖。
- **OpenAI 兼容网关**：把账号变成 `/v1` 接口，多号轮转、每日上限、接口地址/密钥管理，右侧用量统计与使用提示。
- **分享·日志**：复制分享文案/接口地址，查看运行日志概览与最近请求。

## 工作原理

```
第三方客户端 (Cherry Studio / ChatBox / Cline / Dify)
        │  Responses API / Chat Completions
        ▼
127.0.0.1:15722  Rust 本地网关（本项目）
        │  转发 + 模型名映射 + Bearer 鉴权
        ▼
https://www.workbuddy.cn/v2/chat/completions  （WorkBuddy 账号额度）
```

- 客户端用 `wire_api = "responses"` 时，网关把 OpenAI Responses API 翻译成 Chat Completions，再转成 Responses 的 SSE 事件流回给客户端（与 cockpit-tools 的 Rust 引擎同思路）。
- 客户端用 `wire_api = "chat"` 时，网关直接透传（只改模型名与鉴权）。
- 网关忽略来自客户端的 Authorization，始终用账号池里配置的 WorkBuddy Bearer Token 去上游。

## 功能

- **多账号账号池**：加权随机选号（积分 + 闲置补偿）、顺序、纯随机三种策略；429 软冷却指数退避、402/403 硬冷却、404 短冷却、连续失败熔断、会话粘性、单账号在途限制、积分保底；一键「刷新余额」
- **账号切换（真实切号）**：参考 cockpit-tools，改写官方登录态文件 `workbuddy-desktop.info`，用 WorkBuddy 官方 AES-256-GCM 密钥（通过运行官方 Electron 客户端获取）加密 accessToken/refreshToken/nickname 等敏感字段；支持「本机导入」读取当前登录态 token
- **Token 续期（保活）**：调用官方 `https://copilot.tencent.com/v2/plugin/auth/token/refresh` 接口，用 refresh_token 换新 access_token，并将新凭证保存到网关账号池。官方客户端的登录态由「切换账号」单独更新。
- **API Key 管理**：`sk-` 密钥创建/禁用/删除，配额（USD）、5h/1d/7d 限速、过期时间、IP 白/黑名单、请求计数与用量回记；与主密钥（master_key）并存，两者皆空时不鉴权
- **用量统计**：按小时/账号/模型聚合请求数、错误、Prompt/Completion/Total token、延迟、估算积分（倍率 × token/1000），`usage.json` 持久化
- **运行日志**：请求级元数据（时间、账号、模型、状态、耗时、TTFB、token、来源 IP/UA），内存最近 100 条 + `request-logs/requests-YYYY-MM-DD.jsonl` 按天归档，保留天数/体积自动清理
- **任务中心**：一键「签到」、成长任务扫描、一键完成（接受全部未接受任务 → 领取达标未领奖励 → 领新手礼包/活动补偿），逐账号返回签到/接受/领取/积分/能量结果
- 配置 WorkBuddy Token 与 Base URL，一键「测试连接」验证 Token 是否可用
- 一键「从日志提取 Token」：扫描 `~/.workbuddy/logs` 里出现过的 Bearer Token（尽力而为）
- 本地网关启停、端口设置、wire API 切换、开机自启
- 模型映射表：客户端模型名 → WorkBuddy 上游模型 id（默认内置 13 条，含积分倍率）
- 仪表盘查看网关状态、端点、账号是否就绪

> 注意：本项目已**移除 Codex 注入**功能。「切换」按钮直接切换 WorkBuddy 桌面版当前使用的账号：改写官方登录态文件 `workbuddy-desktop.info`（Windows 为 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`），用 WorkBuddy 官方 AES-256-GCM 密钥加密 accessToken/refreshToken 等敏感字段，重启 WorkBuddy 生效。切号机制参考 [cockpit-tools](https://github.com/jlcodes99/cockpit-tools)。

## 快速开始

### 前置要求

- Node.js 18+（已在 24 验证）
- npm 9+
- Rust 稳定版（已在 1.97 验证）

### 开发模式

```bash
npm install
npm run tauri:dev
```

### 构建 Windows 安装包

```bash
npm run tauri:build
```

产物在 `src-tauri/target/release/bundle/nsis/`。

## 使用步骤

1. **加账号**：在「账号池」里添加 WorkBuddy 账号（昵称/UID/域/Token），可点「从日志提取 Token」；保存后点「刷新余额」拉取积分。
2. **（可选）建 API Key**：在「API Keys」里创建 `sk-` key，Codex 请求时用它做 Bearer 鉴权。
3. **启动网关**：默认端口 `15722`，默认 `responses` wire API。
4. **注入 Codex**：进「Codex 注入」点「写入 Codex 配置」，会写入：

   ```toml
   [model_providers.workbuddy]
   name = "WorkBuddy"
   base_url = "http://127.0.0.1:15722/v1"
   wire_api = "responses"
   experimental_bearer_token = "PROXY_MANAGED"
   ```

   若 Codex 需要带 key，把 `experimental_bearer_token` 换成你在「API Keys」创建的 key（或配置 `env_key`）。

5. 重启/刷新 Codex，在模型选择器里选 **WorkBuddy** 下的模型（如 `workbuddy-fast`）即可。
6. 用量、日志、任务中心在左侧对应面板查看。

## 默认模型映射

| Codex 模型名 | WorkBuddy 上游 id | 说明 |
| --- | --- | --- |
| workbuddy-fast | fast-model | 快速（x0.15） |
| workbuddy-balanced | balanced-model | 均衡（x0.50） |
| workbuddy-deep | deep-model | 深度（x1.00） |
| deepseek-v4-pro | deepseek-v4-pro | DeepSeek-V4-Pro |
| glm-5.3 / glm-5.3-flash | glm-5.3 / glm-5.3-flash | GLM |
| kimi-k3 / kimi-k2-thinking | kimi-k3-1 / kimi-k2-thinking | Kimi |
| minimax-m2.5 / minimax-m2.7 | minimax-m2.5 / minimax-m2.7 | MiniMax |
| hy4-preview | hy4-preview-x | Hy4 |

可在「模型映射」页自行增删改。

## 目录结构

```
├── src/                    # React 前端
│   ├── components/         # 面板：仪表盘/账号池/APIKeys/网关/模型/用量/日志/任务/Codex
│   ├── lib/commands.ts     # Tauri invoke 封装
│   ├── stores/appStore.ts  # Zustand
│   ├── i18n/               # 中英文案
│   └── types.ts
└── src-tauri/              # Rust 后端
    └── src/
        ├── gateway/        # axum 网关（鉴权 + 账号池选号 + chat 透传 + responses 翻译）
        ├── pool.rs         # 账号池（选号/冷却/熔断/粘性）
        ├── apikey.rs       # API Key 存储与鉴权
        ├── reqlog.rs       # 请求日志（内存 + JSONL 归档）
        ├── usage.rs        # 用量统计（小时桶）
        ├── tasks.rs        # 任务中心（签到/成长任务/一键完成）
        ├── codex.rs        # 写 Codex config.toml（toml_edit）
        ├── config.rs       # 本地配置持久化
        └── commands.rs     # Tauri 命令
```

## 数据与安全

- 配置保存在 `~/.workbuddy-codex-gateway/config.json`（含账号 Token，**不要提交到 git**，已在 `.gitignore` 排除）。
- `apikeys.json`（API Keys）、`usage.json`（用量）、`request-logs/*.jsonl`（请求日志）同目录存放。
- 网关只监听 `127.0.0.1`，不对外暴露。
- Token 只在本机使用；请求日志只记元数据，不写提示词/响应正文/Authorization。

## 已知限制

- WorkBuddy 上游接口（`/v2/chat/completions`）是账号绑定的私域接口，Token 可能过期。过期后重新提取/粘贴即可；自动刷新 token 暂未实现。
- 账号池运行时状态（冷却/熔断）在内存中，重启应用后归零；积分余额需点「刷新余额」重新拉取。
- 任务中心覆盖「签到 + 全量接受 + 达标领奖 + 领礼包/补偿」；17 项行为类成长任务的事件上报链未逐条实现，进度需真实使用产生。
- Responses API 的多轮续接依赖 Codex 每次发完整 `input`；`previous_response_id` 的增量续接暂不支持。
- 修改 Token / 模型映射 / 端口 / 账号池配置后需「保存」并重启网关生效。

## 许可

仅供个人学习与研究使用，请遵守 WorkBuddy 与 Codex 的服务条款。本项目与腾讯、OpenAI 无任何关联。


