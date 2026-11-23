//! Typed owner confirmation. The phrase must be typed exactly, on a terminal:
//! piped input (`yes |`, scripts, agents) is refused.
use std::collections::VecDeque;
use std::io::{BufRead, IsTerminal, Write};

pub trait Confirm {
    /// Show `what`, ask the owner to type `phrase`; true only on an exact match.
    fn confirm(&mut self, what: &str, phrase: &str) -> bool;
}

/// Interactive terminal confirmation.
pub struct Terminal;

impl Confirm for Terminal {
    fn confirm(&mut self, what: &str, phrase: &str) -> bool {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            println!("REFUSED: confirmation needs an interactive terminal (stdin is not a TTY).");
            return false;
        }
        println!("\n{what}");
        print!("Type exactly `{phrase}` to proceed (anything else aborts): ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            return false;
        }
        let ok = line.trim_end_matches(['\n', '\r']) == phrase;
        println!("{}", if ok { "confirmed" } else { "ABORTED" });
        ok
    }
}

/// Test double: answers from a script and records what was asked.
#[derive(Default)]
pub struct Scripted {
    pub answers: VecDeque<bool>,
    pub asked: Vec<String>,
}

impl Scripted {
    pub fn new(answers: &[bool]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            asked: Vec::new(),
        }
    }
}

impl Confirm for Scripted {
    fn confirm(&mut self, _what: &str, phrase: &str) -> bool {
        self.asked.push(phrase.to_string());
        self.answers.pop_front().unwrap_or(false)
    }
}
