use anyhow::{Context, anyhow, bail};
use fs_err::read_dir;
use regex::Regex;
use rusqlite::{Connection, params};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    ffi::OsStr,
    fmt::Formatter,
    path::{Path, PathBuf},
    process::Command,
};

pub mod tee_helper;

#[derive(serde::Deserialize, Debug)]
/// 当前练习合集的配置。
pub struct ExercisesConfig {
    /// 包含练习的目录路径，相对于仓库根目录。
    #[serde(default = "default_exercise_dir")]
    exercises_dir: PathBuf,
    /// 用于验证练习是否正确的命令。
    #[serde(default)]
    verification: Vec<Verification>,
    /// 在运行验证命令之前不尝试构建项目。
    #[serde(default)]
    pub skip_build: bool,
}

#[derive(serde::Deserialize, Debug)]
/// 特定练习的配置。
pub struct ExerciseConfig {
    /// 用于验证此练习的命令。
    /// 会覆盖合集配置中指定的验证命令（如果有的话）。
    #[serde(default)]
    pub verification: Vec<Verification>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Verification {
    /// 用于验证练习是否正确的命令。
    pub command: String,
    /// 传递给验证命令的参数。
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_exercise_dir() -> PathBuf {
    PathBuf::from("exercises")
}

impl ExercisesConfig {
    pub fn load() -> Result<Self, anyhow::Error> {
        let root_path = get_git_repository_root_dir()
            .context("无法确定当前 git 仓库的根路径")?;
        let exercises_config_path = root_path.join(".wr.toml");
        let exercises_config = fs_err::read_to_string(&exercises_config_path).context(
            "无法读取当前练习合集的配置文件",
        )?;
        let mut exercises_config: ExercisesConfig = toml::from_str(&exercises_config).with_context(|| {
            format!(
                "无法解析当前练习合集的配置文件 `{}`",
                exercises_config_path.to_string_lossy()
            )
        })?;
        // 练习目录的路径是相对于仓库根目录的。
        exercises_config.exercises_dir = root_path.join(&exercises_config.exercises_dir);
        Ok(exercises_config)
    }

    /// 当前练习合集中包含练习的目录路径。
    pub fn exercises_dir(&self) -> &Path {
        &self.exercises_dir
    }

    /// 用于验证练习是否正确的命令。
    /// 如果为空，`wr` 会默认使用 `cargo test`。
    pub fn verification(&self) -> &[Verification] {
        &self.verification
    }
}

/// 获取当前 git 仓库根目录的路径。
pub fn get_git_repository_root_dir() -> Result<PathBuf, anyhow::Error> {
    let cmd = Command::new("git")
        .args(["rev-parse", "--show-cdup"])
        .output()
        .context("运行 `git` 命令（`git rev-parse --show-cdup`）失败，无法确定当前 git 仓库的根路径")?;
    if cmd.status.success() {
        let path = String::from_utf8(cmd.stdout)
            .context("当前 git 仓库的根路径不是有效的 UTF-8 编码")?;
        Ok(path.trim().into())
    } else {
        Err(anyhow!(
            "无法确定当前 git 仓库的根路径"
        ))
    }
}

pub struct ExerciseCollection {
    exercises_dir: PathBuf,
    connection: Connection,
    exercises: BTreeSet<ExerciseDefinition>,
}

impl ExerciseCollection {
    pub fn new(exercises_dir: PathBuf) -> Result<Self, anyhow::Error> {
        let chapters = read_dir(&exercises_dir)
            .context("无法读取练习目录")?
            .filter_map(|entry| {
                let Ok(entry) = entry else {
                    return None;
                };
                let Ok(file_type) = entry.file_type() else {
                    return None;
                };
                if file_type.is_dir() {
                    Some(entry)
                } else {
                    None
                }
            });
        let exercises: BTreeSet<ExerciseDefinition> = chapters
            .flat_map(|entry| {
                let chapter_name = entry.file_name();
                read_dir(entry.path()).unwrap().map(move |f| {
                    let exercise = f.unwrap();
                    (chapter_name.to_owned(), exercise.file_name())
                })
            })
            .filter_map(|(c, k)| ExerciseDefinition::new(&c, &k).ok())
            .collect();

        let db_path = exercises_dir.join("progress.db");
        // 打开数据库（如果不存在则创建）。
        let connection = Connection::open(db_path)
            .context("无法创建用于跟踪进度的 SQLite 数据库")?;
        // 确保所有表都已初始化
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS open_exercises (
                chapter TEXT NOT NULL,
                exercise TEXT NOT NULL,
                solved INTEGER NOT NULL,
                PRIMARY KEY (chapter, exercise)
            )",
                [],
            )
            .context("无法初始化用于跟踪进度的 SQLite 数据库")?;

        Ok(Self {
            connection,
            exercises_dir,
            exercises,
        })
    }

    pub fn n_opened(&self) -> Result<usize, anyhow::Error> {
        let err_msg = "无法确定已打开练习的数量";
        let mut stmt = self
            .connection
            .prepare("SELECT COUNT(*) FROM open_exercises")
            .context(err_msg)?;
        stmt.query_row([], |row| row.get(0)).context(err_msg)
    }

    /// 返回一个遍历所有已打开练习的迭代器。
    pub fn opened(&self) -> Result<BTreeSet<OpenedExercise>, anyhow::Error> {
        opened_exercises(&self.connection)
    }

    /// 按预期顺序返回下一个应该打开的练习。
    pub fn next(&mut self) -> Result<Option<ExerciseDefinition>, anyhow::Error> {
        let opened = opened_exercises(&self.connection)?
            .into_iter()
            .map(|e| e.definition)
            .collect();
        let unsolved = self
            .exercises
            .difference(&opened)
            .cloned()
            .collect::<BTreeSet<_>>();
        for next in unsolved {
            if next.exists(&self.exercises_dir) {
                return Ok(Some(next));
            } else {
                self.close(&next)?;
            }
        }
        Ok(None)
    }

    /// 在数据库中记录某个练习已通过，以便下次跳过。
    pub fn mark_as_solved(&self, exercise: &ExerciseDefinition) -> Result<(), anyhow::Error> {
        self.connection
            .execute(
                "UPDATE open_exercises SET solved = 1 WHERE chapter = ?1 AND exercise = ?2",
                params![exercise.chapter(), exercise.exercise(),],
            )
            .context("无法将练习标记为已解决")?;
        Ok(())
    }

    /// 在数据库中记录某个练习未通过，以便下次不会跳过。
    pub fn mark_as_unsolved(&self, exercise: &ExerciseDefinition) -> Result<(), anyhow::Error> {
        self.connection
            .execute(
                "UPDATE open_exercises SET solved = 0 WHERE chapter = ?1 AND exercise = ?2",
                params![exercise.chapter(), exercise.exercise(),],
            )
            .context("无法将练习标记为未解决")?;
        Ok(())
    }

    /// 打开指定的练习。
    pub fn open(&mut self, exercise: &ExerciseDefinition) -> Result<(), anyhow::Error> {
        if !self.exercises.contains(exercise) {
            bail!("你尝试打开的练习不存在")
        }
        self.connection
            .execute(
                "INSERT OR IGNORE INTO open_exercises (chapter, exercise, solved) VALUES (?1, ?2, 0)",
                params![exercise.chapter(), exercise.exercise(),],
            )
            .context("无法打开下一个练习")?;
        Ok(())
    }

    /// 关闭指定的练习。
    pub fn close(&mut self, exercise: &ExerciseDefinition) -> Result<(), anyhow::Error> {
        self.connection
            .execute(
                "DELETE FROM open_exercises WHERE chapter = ?1 AND exercise = ?2",
                params![exercise.chapter(), exercise.exercise(),],
            )
            .context("无法关闭练习")?;
        Ok(())
    }

    /// 打开下一个练习，假定我们按顺序进行。
    pub fn open_next(&mut self) -> Result<ExerciseDefinition, anyhow::Error> {
        let Some(next) = self.next()? else {
            bail!("没有更多练习可以打开了")
        };
        self.open(&next)?;
        Ok(next)
    }

    /// 包含所有练习章节和练习的目录。
    pub fn exercises_dir(&self) -> &Path {
        &self.exercises_dir
    }

    /// 按预期完成顺序遍历合集中的练习。
    /// 返回已打开和未打开的练习。
    pub fn iter(&self) -> impl Iterator<Item = &ExerciseDefinition> {
        self.exercises.iter()
    }
}

/// 返回所有已打开练习的集合。
fn opened_exercises(connection: &Connection) -> Result<BTreeSet<OpenedExercise>, anyhow::Error> {
    let err_msg = "无法获取已开始练习的列表";
    let mut stmt = connection
        .prepare("SELECT chapter, exercise, solved FROM open_exercises")
        .context(err_msg)?;
    let opened_exercises = stmt
        .query_map([], |row| {
            let chapter = row.get_ref_unwrap(0).as_str().unwrap();
            let exercise = row.get_ref_unwrap(1).as_str().unwrap();
            let solved = row.get_ref_unwrap(2).as_i64().unwrap();
            let solved = if solved == 0 { false } else { true };
            let definition = ExerciseDefinition::new(chapter.as_ref(), exercise.as_ref())
                .expect("数据库中存储了无效的练习");
            Ok(OpenedExercise { definition, solved })
        })
        .context(err_msg)?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(opened_exercises)
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExerciseDefinition {
    chapter_name: String,
    chapter_number: u16,
    name: String,
    number: u16,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenedExercise {
    pub definition: ExerciseDefinition,
    pub solved: bool,
}

impl PartialOrd for OpenedExercise {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.definition.partial_cmp(&other.definition)
    }
}

impl Ord for OpenedExercise {
    fn cmp(&self, other: &Self) -> Ordering {
        self.definition.cmp(&other.definition)
    }
}

impl PartialOrd for ExerciseDefinition {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let ord = self
            .chapter_number
            .cmp(&other.chapter_number)
            .then(self.number.cmp(&other.number));
        Some(ord)
    }
}

impl Ord for ExerciseDefinition {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl PartialEq<OpenedExercise> for ExerciseDefinition {
    fn eq(&self, other: &OpenedExercise) -> bool {
        self == &other.definition
    }
}

impl PartialOrd<OpenedExercise> for ExerciseDefinition {
    fn partial_cmp(&self, other: &OpenedExercise) -> Option<Ordering> {
        self.partial_cmp(&other.definition)
    }
}

impl ExerciseDefinition {
    pub fn new(chapter_dir_name: &OsStr, exercise_dir_name: &OsStr) -> Result<Self, anyhow::Error> {
        fn parse(dir_name: &OsStr, type_: &str) -> Result<(String, u16), anyhow::Error> {
            // TODO: 将正则编译为静态变量，只编译一次。
            let re = Regex::new(r"(?P<number>\d{2})_(?P<name>\w+)").unwrap();

            let dir_name = dir_name.to_str().ok_or_else(|| {
                anyhow!(
                    "{type_} 的名称必须是有效的 UTF-8 文本，但 {:?} 不是",
                    dir_name
                )
            })?;
            match re.captures(&dir_name) {
                None => bail!("无法将 `{dir_name:?}` 解析为 {type_}（格式应为 <NN>_<name>）。",),
                Some(s) => {
                    let name = s["name"].into();
                    let number = s["number"].parse().unwrap();
                    Ok((name, number))
                }
            }
        }

        let (name, number) = parse(exercise_dir_name, "练习")?;
        let (chapter_name, chapter_number) = parse(chapter_dir_name, "章节")?;

        Ok(ExerciseDefinition {
            chapter_name,
            chapter_number,
            name,
            number,
        })
    }

    /// 当前练习的 `Cargo.toml` 文件路径。
    pub fn manifest_path(&self, exercises_dir: &Path) -> PathBuf {
        self.manifest_folder_path(exercises_dir).join("Cargo.toml")
    }

    /// 包含当前练习 `Cargo.toml` 文件的目录路径。
    pub fn manifest_folder_path(&self, exercises_dir: &Path) -> PathBuf {
        exercises_dir.join(self.chapter()).join(self.exercise())
    }

    /// 当前练习的配置（如果有的话）。
    pub fn config(&self, exercises_dir: &Path) -> Result<Option<ExerciseConfig>, anyhow::Error> {
        let exercise_config = self.manifest_folder_path(exercises_dir).join(".wr.toml");
        if !exercise_config.exists() {
            return Ok(None);
        }
        let exercise_config = fs_err::read_to_string(&exercise_config).context(format!(
            "无法读取练习 `{}` 的配置文件",
            self.exercise()
        ))?;
        let exercise_config: ExerciseConfig =
            toml::from_str(&exercise_config).with_context(|| {
                format!(
                    "无法解析练习 `{}` 的配置文件",
                    self.exercise()
                )
            })?;
        Ok(Some(exercise_config))
    }

    /// 包含此练习的章节的编号+名称。
    pub fn chapter(&self) -> String {
        format!("{:02}_{}", self.chapter_number, self.chapter_name)
    }

    /// 此练习的编号+名称。
    pub fn exercise(&self) -> String {
        format!("{:02}_{}", self.number, self.name)
    }

    /// 此练习的编号。
    pub fn exercise_number(&self) -> u16 {
        self.number
    }

    /// 包含此练习的章节编号。
    pub fn chapter_number(&self) -> u16 {
        self.chapter_number
    }

    /// 验证练习是否存在。
    /// 可能在课程更新后已从仓库中移除。
    pub fn exists(&self, exercises_dir: &Path) -> bool {
        self.manifest_path(exercises_dir).exists()
    }
}

impl std::fmt::Display for ExerciseDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({:02}) {} - ({:02}) {}",
            self.chapter_number, self.chapter_name, self.number, self.name
        )
    }
}
