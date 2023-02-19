use std::{
    io::stdout,
    process::{Child, Command, Stdio},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor, Stylize},
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
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
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

    let Some(task) = select_task(&yaml) else {
        return
    };
    create_process(task).wait().expect("Process failed");

    if task.confirmation {
        println!();
        println!("   Task completed. Press Enter to continue");
        println!();
        while read_key_code() != KeyCode::Enter {}
    }
}

fn create_process(task: &Task) -> Child {
    Command::new(task.cmd.clone())
        .args(task.args.clone())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Unable to start")
}

fn read_key_code() -> KeyCode {
    let KeyEvent { code, .. } = read_key_event();
    code
}

fn read_key_event() -> KeyEvent {
    enable_raw_mode().unwrap();
    let key_code = loop {
        if let Ok(true) = event::poll(Duration::from_secs(60)) {
            let event = event::read().unwrap();
            if let Event::Key(e) = event {
                break e;
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
        println!("    {}", "SELECT A TASK".stylize().grey());
        println!();
        for task in tasks {
            println!(
                "    <{}>  {:10}",
                task.key.stylize().green().bold(),
                task.name.clone().stylize().white().bold()
            );
        }
        println!();
        println!("    <{}>  {:10}", "q".stylize().red(), "quit");

        if let Some(e) = error.take() {
            println!("\n   {}\n", e.stylize().red());
        }

        let KeyEvent {
            code, modifiers, ..
        } = read_key_event();
        match code {
            KeyCode::Char('q') => return None,
            KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => return None,

            KeyCode::Char(ch) => {
                let task = tasks.iter().find(|t| t.key == ch);
                if task.is_some() {
                    return task;
                } else {
                    error = Some(format!("No task for key: {}", ch));
                }
            }
            _ => {}
        }
    }
}
