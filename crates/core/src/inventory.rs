use std::path::PathBuf;

use serde::Serialize;

/// 清单的一行：一个受管文件的源路径 ↔ 目标路径 + 状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventoryEntry {
    /// 源目录里的绝对路径。
    pub source: PathBuf,
    /// 目标文件的绝对路径（`home` 下的镜像位置）。
    pub target: PathBuf,
    pub status: Status,
}

/// 一行此刻的状态，全部由「源目录 git 事实 × 目标文件比较」推导，见 `state::derive`。
///
/// 变体名保持英文（中文只活在前端），序列化成 `"InSync"` 等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    /// 冲突：本地未提交改动撞上远端对同一路径的改动（或已留下未合并路径）。
    Conflict,
    /// 待提交：源目录里这个文件有未提交改动。
    PendingCommit,
    /// 待拉取：远端改了这个文件，本地还没拉（快进得动）。
    PendingPull,
    /// 目标缺失
    TargetMissing,
    /// 一致
    InSync,
    /// 待应用
    Pending,
}

/// 一行的 unified diff。文本由 Core 用 `similar` 生成，前端只渲染与折叠。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDiff {
    /// 被点开的那一行的目标路径。
    pub target: PathBuf,
    /// 该行此刻的状态，决定 diff 比较的是哪两侧。
    pub status: Status,
    /// unified diff 文本；空字符串表示两侧没有差异。
    pub text: String,
}

/// 顶部状态条：分支、ahead/behind、未提交改动数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    /// `git status --porcelain` 的行数（含未跟踪文件）。
    pub dirty: u32,
}
