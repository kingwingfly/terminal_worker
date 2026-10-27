//! The terminal itself: help, man and clear.

use clap::Parser;

use super::{Manual, Section};

/// list the commands
///
/// Lists the commands by section, a line each, or shows COMMAND's help, as COMMAND -h does; man
/// COMMAND opens its manual page.
#[derive(Parser, Debug)]
#[command(name = "help")]
pub struct Help {
    #[arg(value_name = "COMMAND")]
    pub command: Option<String>,
}
impl Manual for Help {
    const SECTION: Section = Section::Terminal;
    const SEE: &'static [&'static str] = &["man"];
}

/// read the manual
///
/// Opens COMMAND's manual page, or, without one, the index and the overview of permissions,
/// links, pipes and keys. It describes what this terminal does, not a whole Linux system.
#[derive(Parser, Debug)]
#[command(name = "man")]
pub struct Man {
    #[arg(value_name = "COMMAND")]
    pub command: Option<String>,
}
impl Manual for Man {
    const SECTION: Section = Section::Terminal;
    const EXAMPLES: &'static [(&'static str, &'static str)] =
        &[("man ln", "one page"), ("man", "the index")];
    const SEE: &'static [&'static str] = &["help"];
}

/// clear the terminal
///
/// Clears this tab's output; nothing else changes.
#[derive(Parser, Debug)]
#[command(name = "clear")]
pub struct Clear {}
impl Manual for Clear {
    const SECTION: Section = Section::Terminal;
}
