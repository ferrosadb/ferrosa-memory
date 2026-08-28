//! Embed the commit this binary was built from.
//!
//! Without it a Sentry event carries only a semver version, and two builds of
//! the same version from different commits are indistinguishable — which is
//! exactly the question asked when a report arrives: is this the build that
//! already has the fix?
//!
//! Order matters. `FERROSA_BUILD_SHA` from the environment wins, because a
//! release is built from a tarball or a container where there is no `.git` to
//! ask, and CI knows the commit it checked out. Only then does it shell out to
//! git, for a developer build. `unknown` is the honest last resort: a made-up
//! or stale SHA is worse than none, because it would be believed.

use std::process::Command;

fn main() {
    let sha = std::env::var("FERROSA_BUILD_SHA")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(git_short_sha)
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=FERROSA_BUILD_SHA={sha}");

    // Rebuild when the commit moves, or the stamp goes stale the moment
    // anyone commits without touching this crate.
    println!("cargo:rerun-if-env-changed=FERROSA_BUILD_SHA");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    // A dirty tree is a build nobody else can reproduce, and saying so costs
    // one character.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|out| !out.stdout.is_empty());
    if sha.is_empty() {
        None
    } else if dirty {
        Some(format!("{sha}-dirty"))
    } else {
        Some(sha)
    }
}
