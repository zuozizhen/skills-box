# SkillsBox

<p align="center">
  <img src="src-tauri/icons/128x128.png" alt="SkillsBox Logo" width="96" />
</p>

SkillsBox 是一个面向普通用户的 macOS 桌面应用，用来查看、管理和理解你安装的各类 AI Skills。

![SkillsBox 应用预览](public/skills-box-show.png)

## 核心功能

### 1. 多来源自动发现与实时同步

SkillsBox 会自动扫描并聚合多类 Skills 来源，包括全局目录、常见 AI 工具目录、项目内目录，以及 `skills list --json` 的结果。

- 自动识别新增 Skill，列表实时更新
- 监听文件变化，内容变更后自动刷新索引
- 支持手动刷新，随时强制重建当前快照
- 对重复 Skill 做去重合并，避免同一技能多次出现

怎么用：
1. 打开应用后等待首次扫描完成。
2. 把新的 Skill 放到常用目录（如项目 `skills/` 或全局目录）。
3. 回到 SkillsBox，自动同步或点击“刷新”立即生效。

### 2. 结构化详情页 + 原文对照阅读

每个 Skill 都有结构化详情，降低阅读门槛，同时保留原始内容用于核对。

- 左侧支持搜索，按名称、路径、原文、AI 总结内容联合检索
- 右侧展示技能路径、定义文件、来源信息、摘要与详细说明
- 支持在“AI总结 / 原始全文（SKILL.md）”之间切换
- 支持一键复制定义路径，便于粘贴给任意 AI 工具直接调用

怎么用：
1. 在搜索框输入关键词快速定位 Skill。
2. 选中 Skill 后先看 AI 总结快速理解用途。
3. 需要确认细节时切到原始全文查看 `SKILL.md`。

### 3. AI 总结工作流（新增自动、补全、重总结）

SkillsBox 内置 DeepSeek 驱动的总结能力，可把英文或复杂描述转成更易读的中文说明。

- 新发现 Skill 可自动补全 AI 总结
- 支持“补全未总结”，只处理缺失项
- 支持“重总结当前 Skill”精修单条内容
- 支持“重总结全部 Skill”批量覆盖并展示进度
- 全流程后台执行，不阻塞主界面操作

怎么用：
1. 在设置里配置并测试 API Key。
2. 点击“补全未总结”生成首轮内容。
3. 对不满意的条目，使用“重总结当前 Skill”单独重跑。

### 4. 收藏与菜单栏高频直达

针对高频技能，SkillsBox 支持收藏和菜单栏快捷入口。

- Skill 可一键收藏，形成个人常用集
- 菜单栏展示收藏项，便于随时调用
- 点击菜单栏收藏项可直接复制技能定义路径
- 收藏列表保存在本地，重启后保持不丢失

怎么用：
1. 在 Skill 卡片上点击收藏。
2. 从菜单栏打开收藏列表。
3. 点击目标 Skill，直接复制路径并粘贴给 AI 使用。

### 5. 应用内版本检查与更新

SkillsBox 支持在应用内检查新版本并执行更新。

- 可手动检查更新
- 检测到新版本后可直接执行更新流程
- 提供当前版本与目标版本提示，便于确认

## AI 配置（DeepSeek）

你只需要两步：

1. 去 [DeepSeek 开放平台](https://platform.deepseek.com/api_keys) 申请一个 API Key。
2. 在 SkillsBox 设置里填入 Key 并测试通过。

## 本地运行

### 环境要求

- macOS
- Node.js 18+
- Rust（stable）
- Tauri 2 所需系统依赖

### 启动步骤

```bash
npm ci
npm run tauri dev
```

## GitHub Description (EN)

SkillsBox is a macOS desktop app for discovering, organizing, and understanding AI Skills across global and project directories, with AI-powered summaries, source-view comparison, favorites, and tray-based quick actions.

## License

MIT License

Copyright (c) 2026 SkillsBox Contributors
