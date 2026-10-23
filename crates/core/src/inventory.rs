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

/// 由「源文件 × 目标文件」两者比较得出的状态。
///
/// 本票只实现纯磁盘事实推导出的三种；`待提交 / 待拉取 / 冲突` 需要 git 状态与
/// ahead-behind，由「清单状态与 diff」那张票补齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    /// 一致
    InSync,
    /// 目标缺失
    TargetMissing,
    /// 待应用
    Pending,
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
