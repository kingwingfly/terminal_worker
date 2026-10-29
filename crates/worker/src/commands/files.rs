//! cp, which the server runs until files are written through descriptors (phase 3 of the
//! system calls), when it moves to user space with the rest.

use serde_json::json;
use terminal_worker_core::cli::{files as args, parse};
use terminal_worker_core::permissions::*;

use super::{Context, ServerCommand, joined};

/// Into `dest` if it is, or leads to, a directory: the directory and true; otherwise `dest` as
/// a name, a link there being replaced or written through. Only where `who` may look is it told
/// what is there.
fn destination(ctx: &Context, dest: &str) -> Result<(String, bool), String> {
    let followed = ctx.path(dest)?;
    ctx.db.traverse(ctx.who, &followed)?;
    if ctx.db.node(&followed).is_some_and(|n| n.directory) {
        return Ok((followed, true));
    }
    Ok((ctx.entry(dest)?, false))
}

pub struct Cp;
impl ServerCommand for Cp {
    fn name(&self) -> &'static str {
        "cp"
    }
    fn run(&self, ctx: &mut Context) -> Result<String, String> {
        let cp = parse::<args::Cp>(ctx.args)?;
        let options = CopyOptions {
            recursive: cp.archive || cp.recursive,
            replace: !cp.no_clobber && !cp.interactive,
            force: cp.force,
            dereference: cp.dereference,
            links: cp.archive || cp.links,
            preserve: cp.archive || cp.preserve,
            xattrs: cp.archive,
        };
        // A link named on the command line is followed without -r, and with -L or -H; otherwise
        // copied as one (-P, also in -d and -a).
        let follow = cp.dereference
            || cp.command_line
            || !cp.no_dereference && !options.recursive && !options.links;
        let (dest, into) = destination(ctx, &cp.dest)?;
        if cp.sources.len() > 1 && !into {
            return Err(format!("cp: target '{dest}' is not a directory"));
        }
        let mut done = Vec::new();
        for value in &cp.sources {
            let src = if follow {
                ctx.path(value)
            } else {
                ctx.entry(value)
            };
            let result = src.and_then(|src| {
                let target = if into {
                    joined(&dest, src.rsplit('/').next().unwrap_or_default())
                } else {
                    dest.clone()
                };
                // Files still uploading are copied once whole: Store::copy refuses them.
                let skipped = ctx.db.copy(ctx.who, &src, &target, &options)?;
                let whole = skipped.contains(&target);
                if cp.interactive {
                    // What a directory holds is at the same place below the source.
                    ctx.existing.extend(
                        skipped
                            .into_iter()
                            .map(|t| json!([format!("{src}{}", &t[target.len()..]), t])),
                    );
                }
                Ok((!whole).then(|| format!("'{src}' -> '{target}'")))
            });
            if let Some(Some(line)) = ctx.each(result).filter(|_| cp.verbose) {
                done.push(line);
            }
        }
        Ok(done.join("\n"))
    }
}
