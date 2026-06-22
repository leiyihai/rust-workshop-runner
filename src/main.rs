use clap::{Parser, Subcommand};
use fs_err::PathExt;
use read_input::prelude::*;
use std::{ffi::OsString, path::Path};
use wr::{
    ExerciseCollection, ExerciseDefinition, ExercisesConfig, OpenedExercise, Verification,
    tee_helper::run_and_capture,
};
use yansi::Paint;

/// 一个用于管理测试驱动 Rust 工作坊和教程的小型 CLI 工具。
///
/// 每个练习都附带一组相关的测试。
/// 一组练习称为"合集"。
///
/// 运行 `wr` 会对你在合集中已打开的所有练习执行测试，
/// 以检查你的解答是否正确。
/// 如果全部通过，程序会询问你是否要进入下一个练习。
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Command {
    #[arg(long)]
    /// 编译并运行所有已打开练习的测试，即使它们在之前的运行中已经通过。
    pub recheck: bool,

    #[arg(long)]
    /// 默认情况下，`wr` 会以静默模式运行 `cargo build`，不显示构建过程的日志。
    /// 使用此标志后，这些日志（和进度条）将会显示出来。
    pub verbose: bool,

    #[arg(long)]
    /// 默认情况下，如果所有已打开的练习都通过了测试，`wr` 会询问你是否要打开下一个练习。
    /// 使用此标志后，`wr` 会在所有已打开练习通过测试后自动打开下一个练习，
    /// 并运行新打开练习的测试。如果通过，则继续打开下一个，以此类推。
    pub keep_going: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// 打开指定的练习。
    ///
    /// 你可以提供章节和练习的完整名称，也可以只提供编号。
    ///
    /// 例如 `wr open --chapter 01_structured_logging --exercise 00_intro` 会打开
    /// 位于 `01_structured_logging/00_intro` 的练习。
    /// 同一个练习也可以用 `wr open --chapter 1 --exercise 0` 来打开。
    Open {
        /// 包含该练习的章节名称或编号。
        ///
        /// 例如 `--chapter 01_structured_logging` 和 `--chapter 1` 是等价的。
        #[arg(long)]
        chapter: String,
        /// 练习的名称，或在其所属章节中的编号。
        ///
        /// 例如 `--exercise 00_intro` 和 `--exercise 0` 是等价的。
        #[arg(long)]
        exercise: String,
    },
    /// 运行当前目录中练习的测试。
    /// 如果当前目录不是一个练习，则会报错。
    Check,
}

fn main() -> Result<(), anyhow::Error> {
    let command = Command::parse();
    // 在 Windows 上启用 ANSI 颜色支持（如果支持的话）。
    // 否则完全禁用。
    if !use_ansi_colours() {
        Paint::disable();
    }
    let configuration = ExercisesConfig::load()?;
    let verbose = command.verbose;
    let mut exercises = ExerciseCollection::new(configuration.exercises_dir().to_path_buf())?;

    if let Some(command) = command.command {
        match command {
            Commands::Open { chapter, exercise } => {
                enum Selector {
                    FullName(String),
                    Number(u16),
                }

                impl Selector {
                    fn new(s: String) -> Self {
                        match s.parse::<u16>() {
                            Ok(number) => Selector::Number(number),
                            Err(_) => Selector::FullName(s),
                        }
                    }

                    fn matches(&self, name: &str, number: u16) -> bool {
                        match self {
                            Selector::FullName(s) => s == name,
                            Selector::Number(n) => *n == number,
                        }
                    }
                }

                impl std::fmt::Display for Selector {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        match self {
                            Selector::FullName(s) => write!(f, "{}", s),
                            Selector::Number(n) => write!(f, "{}", n),
                        }
                    }
                }

                let chapter_selector = Selector::new(chapter);
                let exercise_selector = Selector::new(exercise);

                let exercise = exercises.iter().find(|k| {
                    chapter_selector.matches(&k.chapter(), k.chapter_number())
                        && exercise_selector.matches(&k.exercise(), k.exercise_number())
                }).ok_or_else(|| {
                    anyhow::anyhow!("没有匹配 `--chapter {chapter_selector} -- exercise {exercise_selector}` 的练习")
                })?.to_owned();

                exercises.open(&exercise)?;
                print_opened_message(&exercise, exercises.exercises_dir());
            }
            Commands::Check => {
                let current_dir = std::env::current_dir()?.fs_err_canonicalize()?;
                let definition = exercises
                    .iter()
                    .find(|k| {
                        let manifest_folder = k
                            .manifest_folder_path(exercises.exercises_dir())
                            .fs_err_canonicalize()
                            .expect("无法规范化 manifest 目录路径");
                        manifest_folder == current_dir
                    })
                    .ok_or_else(|| anyhow::anyhow!("当前目录不是一个练习"))?;
                if let TestOutcome::Failure { command, details } = verify(
                    &exercises,
                    &definition,
                    configuration.verification(),
                    configuration.skip_build,
                    verbose,
                )? {
                    print_failure_message(&command, &details);
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // 如果没有指定子命令，则验证用户已打开练习的进度。
    if let TestOutcome::Failure { command, details } = seek_the_path(
        &mut exercises,
        command.recheck,
        configuration.verification(),
        configuration.skip_build,
        verbose,
    )? {
        print_failure_message(&command, &details);
        std::process::exit(1);
    };

    // 如果所有已打开的练习都通过了检查，则打开下一个（如果存在的话）。
    while let Some(next_exercise) = exercises.next()? {
        if command.keep_going {
            let next_exercise = exercises
                .open_next()
                .expect("无法打开下一个练习");
            let exercise_outcome = verify(
                &exercises,
                &next_exercise,
                configuration.verification(),
                configuration.skip_build,
                command.verbose,
            )?;
            if let TestOutcome::Failure { command, details } = exercise_outcome {
                print_failure_message(&command, &details);
                std::process::exit(1);
            };
            continue;
        } else {
            println!(
                "\t{}\n",
                info_style().paint(
                    "永恒在我们身前，也在我们身后。你的旅程尚未结束。🍂"
                )
            );

            let open_next = input::<String>()
                .repeat_msg(format!(
                    "是否要打开下一个练习 {}？[y/n] ",
                    next_exercise
                ))
                .err("请回答 yes 或 no。")
                .add_test(|s| parse_bool(s).is_some())
                .get();
            // 这里可以安全地 unwrap，因为输入已经过验证。
            let open_next = parse_bool(&open_next).unwrap();

            if open_next {
                let next_exercise = exercises
                    .open_next()
                    .expect("无法打开下一个练习");
                print_opened_message(&next_exercise, exercises.exercises_dir());
            }
            return Ok(());
        }
    }
    println!(
        "{}\n\t{}\n",
        success_style().paint("\n\t已没有更多任务。"),
        info_style().paint("一只手鼓掌是什么声音（对你而言）？🌟")
    );
    Ok(())
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "yes" | "y" | "是" => Some(true),
        "no" | "n" | "否" => Some(false),
        _ => None,
    }
}

fn seek_the_path(
    exercises: &mut ExerciseCollection,
    recheck: bool,
    verification: &[Verification],
    skip_build: bool,
    verbose: bool,
) -> Result<TestOutcome, anyhow::Error> {
    println!(" \n\n{}", info_style().dimmed().paint("正在运行测试...\n"));
    for exercise in exercises.opened()? {
        let OpenedExercise { definition, solved } = &exercise;
        if !exercise.definition.exists(exercises.exercises_dir()) {
            exercises.close(&definition)?;
            continue;
        }
        if *solved && !recheck {
            println!(
                "{}",
                info_style().paint(format!("\t⏩ {}（跳过复查）", definition))
            );
            continue;
        }
        let exercise_outcome = verify(exercises, &definition, verification, skip_build, verbose)?;
        if let TestOutcome::Failure { command, details } = exercise_outcome {
            return Ok(TestOutcome::Failure { command, details });
        }
    }
    Ok(TestOutcome::Success)
}

fn verify(
    exercises: &ExerciseCollection,
    definition: &ExerciseDefinition,
    verification: &[Verification],
    skip_build: bool,
    verbose: bool,
) -> Result<TestOutcome, anyhow::Error> {
    let exercise_config = definition.config(exercises.exercises_dir())?;
    // 练习专属配置优先于全局配置（如果指定了的话）。
    let verification = exercise_config
        .as_ref()
        .map(|c| c.verification.as_slice())
        .unwrap_or(verification);
    let exercise_outcome = _verify(
        &definition.manifest_path(exercises.exercises_dir()),
        verification,
        skip_build,
        verbose,
    );
    match &exercise_outcome {
        TestOutcome::Success => {
            println!("{}", success_style().paint(format!("\t🚀 {}", definition)));
            exercises.mark_as_solved(&definition)?;
        }
        TestOutcome::Failure { .. } => {
            println!("{}", failure_style().paint(format!("\t❌ {}", definition)));
            exercises.mark_as_unsolved(&definition)?;
        }
    }
    Ok(exercise_outcome)
}

fn _verify(
    manifest_path: &Path,
    verification: &[Verification],
    skip_build: bool,
    verbose: bool,
) -> TestOutcome {
    // 告诉 cargo 输出彩色内容，除非我们在 Windows 上且终端不支持。
    let color_option = if use_ansi_colours() {
        "always"
    } else {
        "never"
    };

    // 先 `cargo build`
    if !skip_build {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("build");
        cmd.arg("--manifest-path");
        cmd.arg(manifest_path);
        cmd.arg("--all-targets");
        cmd.arg("--color");
        cmd.arg(color_option);
        if !verbose {
            cmd.arg("-q");
        }

        if verbose {
            cmd.stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
        }

        let output = cmd.output().expect("构建项目失败");

        if !output.status.success() {
            return TestOutcome::Failure {
                command: format!("{:?}", cmd),
                details: [output.stderr, output.stdout].concat(),
            };
        }
    }

    // 然后运行验证命令。
    {
        let mut verification_commands: Vec<_> = verification
            .iter()
            .map(|v| {
                let mut cmd = std::process::Command::new(&v.command);
                cmd.args(&v.args);
                cmd
            })
            .collect();
        if verification_commands.is_empty() {
            let mut args: Vec<OsString> =
                vec!["test".into(), "--color".into(), color_option.into()];

            if !verbose {
                args.push("-q".into());
            }

            let mut cmd = std::process::Command::new("cargo");
            cmd.args(args);
            verification_commands.push(cmd);
        }
        verification_commands.iter_mut().for_each(|cmd| {
            // 从练习所在目录运行验证命令。
            cmd.current_dir(
                manifest_path
                    .parent()
                    .expect("无法获取 manifest 的父目录"),
            );
        });
        for mut verification_cmd in verification_commands {
            let error_msg = format!("运行失败：`{:?}`", verification_cmd);
            let command_dbg = format!("{:?}", verification_cmd);
            let (status, stderr, stdout) = if verbose {
                let captured = run_and_capture(verification_cmd).expect(&error_msg);
                (captured.status, captured.stderr, captured.stdout)
            } else {
                let output = verification_cmd.output().expect(&error_msg);
                (output.status, output.stderr, output.stdout)
            };

            if !status.success() {
                return TestOutcome::Failure {
                    command: command_dbg,
                    details: [stderr, stdout].concat(),
                };
            }
        }
    }

    TestOutcome::Success
}

#[derive(PartialEq)]
enum TestOutcome {
    Success,
    Failure { command: String, details: Vec<u8> },
}

fn print_opened_message(exercise: &ExerciseDefinition, exercises_dir: &Path) {
    println!(
        "{} {}",
        next_style().paint("\n\t你的前方是"),
        next_style().bold().paint(format!("{exercise}")),
    );
    let relative_path = exercise.manifest_folder_path(exercises_dir);
    let open_msg = format!(
        "\n\t在编辑器中打开 {:?} 并开始吧！\n\t再次运行 `wr` 来编译练习并执行测试。",
        relative_path
    );
    println!("{}", next_style().paint(open_msg));
}

fn print_failure_message(command: &str, details: &[u8]) {
    println!(
        "\n\t{}\n\n运行失败：\n\t{}\n输出：\n{}\n",
        info_style()
            .paint("静思你的方法，然后再回来。山不过是山。\n\n"),
        cargo_style().paint(&command),
        cargo_style().paint(textwrap::indent(
            &String::from_utf8_lossy(details).to_string(),
            "\t"
        ))
    );
}

pub fn info_style() -> yansi::Style {
    yansi::Style::new(yansi::Color::Default)
}
pub fn cargo_style() -> yansi::Style {
    yansi::Style::new(yansi::Color::Default).dimmed()
}
pub fn next_style() -> yansi::Style {
    yansi::Style::new(yansi::Color::Yellow)
}
pub fn success_style() -> yansi::Style {
    yansi::Style::new(yansi::Color::Green)
}
pub fn failure_style() -> yansi::Style {
    yansi::Style::new(yansi::Color::Red)
}

/// 判断我们的终端输出是否应该通过 ANSI 转义码使用颜色。
pub fn use_ansi_colours() -> bool {
    if cfg!(target_os = "windows") {
        Paint::enable_windows_ascii()
    } else {
        true
    }
}
