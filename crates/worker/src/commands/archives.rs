//! Archives and video: zip, unzip, tar and faststart start a job, which a JobRunner does
//! with its container (crate::jobs, crate::runner); the reply names it, for the terminal to
//! follow.

use super::{Context, ServerCommand};

/// Start the job `ctx`'s command asks for.
fn start(ctx: &mut Context) -> Result<String, String> {
    ctx.reply = Some(crate::jobs::start(ctx.db, ctx.who, ctx.owner, ctx.command)?);
    Ok(String::new())
}

pub struct Zip;
impl ServerCommand for Zip {
    fn name(&self) -> &'static str {
        "zip"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        start(ctx)
    }
}

pub struct Unzip;
impl ServerCommand for Unzip {
    fn name(&self) -> &'static str {
        "unzip"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        start(ctx)
    }
}

pub struct Tar;
impl ServerCommand for Tar {
    fn name(&self) -> &'static str {
        "tar"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        start(ctx)
    }
}

pub struct Faststart;
impl ServerCommand for Faststart {
    fn name(&self) -> &'static str {
        "faststart"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        start(ctx)
    }
}
