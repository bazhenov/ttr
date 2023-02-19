use std::{
    io::stdout,
    process::{Command, Stdio},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct Task {
    name: String,
    key: char,
    cmd: Vec<String>,
    #[serde(default)]
    confirmation: bool,
}

struct AlternateScreen;

impl AlternateScreen {
    fn enter() -> Self {
        execute!(stdout(), EnterAlternateScreen).unwrap();
        Self
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        execute!(stdout(), LeaveAlternateScreen).unwrap();
    }
}

fn main() {
    let file = std::fs::File::open("./tasks.yaml").unwrap();
    let yaml: Vec<Task> = serde_yaml::from_reader(file).unwrap();

    if let Some(task) = select_task(&yaml) {
        let mut result = Command::new(task.cmd[0].clone())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Unable to start");
        result.wait().expect("Process failed");

        if task.confirmation {
            println!();
            println!("   Task completed. Press Enter to continue");
            println!();
            while read_key_code() != KeyCode::Enter {}
        }
    }
}

fn read_key_code() -> KeyCode {
    enable_raw_mode().unwrap();
    let key_code = loop {
        if let Ok(true) = event::poll(Duration::from_secs(60)) {
            let event = event::read().unwrap();
            if let Event::Key(KeyEvent { code, .. }) = event {
                break code;
            }
        }
    };
    disable_raw_mode().unwrap();
    key_code
}

fn select_task(tasks: &[Task]) -> Option<&Task> {
    let _alt = AlternateScreen::enter();
    let mut stdout = stdout().lock();

    let mut error: Option<String> = None;
    loop {
        execute!(stdout, Clear(ClearType::All)).unwrap();
        for task in tasks {
            println!("   [{}] {}", task.key, task.name);
        }

        if let Some(e) = error.take() {
            let msg = format!("\n   {}\n", e);
            execute!(
                stdout,
                SetForegroundColor(Color::Red),
                Print(msg),
                ResetColor
            )
            .unwrap();
        }

        match read_key_code() {
            KeyCode::Char('q') => return None,
            KeyCode::Char(ch) => {
                for task in tasks {
                    if ch == task.key {
                        return Some(task);
                    }
                }
                error = Some(format!("No task for key: {}", ch));
            }
            _ => {}
        }
    }
}
