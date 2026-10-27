//! Archives and video: zip, unzip, tar and faststart start a job, which a JobRunner does
//! with its container (crate::jobs, crate::runner); the reply names it, for the terminal to
//! follow.

use super::{Context, ServerCommand};

/// A command that starts a job: zip, unzip, tar or faststart, which crate::jobs::start parses.
pub struct Job(pub &'static str);
impl ServerCommand for Job {
    fn name(&self) -> &'static str {
        self.0
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        ctx.reply = Some(crate::jobs::start(ctx.db, ctx.who, ctx.owner, ctx.command)?);
        Ok(String::new())
    }
}
