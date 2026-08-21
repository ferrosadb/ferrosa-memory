"""A launchd template must not look like an installable plist.

`launchd/com.ferrosa-memory.stack.plist` was a valid-looking plist whose
ProgramArguments held the literal string `__SCRIPT_PATH__`. A copy of it reached
`~/Library/LaunchAgents` without going through the installer, where it sat for
four days looking like a registered job.

launchd cannot exec a literal `__SCRIPT_PATH__`, so the job never ran. Because
the failure mode is "never started" rather than "started and crashed", nothing
reported it: `launchctl list` simply does not mention it, and there are no logs
to be missing. The login-startup it exists to provide had quietly not existed
since the day it was put there, and the only reason it surfaced was an unrelated
audit reading launchd program paths.

The NAME is what makes this safe. `.plist.in` is not a plist: it cannot be
copied into LaunchAgents and mistaken for one, and `cp launchd/*.plist` matches
nothing at all. The installers then assert no placeholder survived substitution,
which catches the other direction -- a template growing a new placeholder that
some installer does not know to replace.
"""

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
LAUNCHD = REPO_ROOT / "launchd"

# Everything that reads a template out of launchd/.
CONSUMERS = [
    "scripts/install-launch-agent.sh",
    "scripts/install-launch-agent-mcp.sh",
    "docs/install-memory.sh",
    ".github/scripts/stage-release-tarball.sh",
]

# The three that write a plist and must prove the substitution happened.
INSTALLERS = [
    "scripts/install-launch-agent.sh",
    "scripts/install-launch-agent-mcp.sh",
    "docs/install-memory.sh",
]


def _placeholders(text):
    """The `__NAME__` tokens in a file."""
    import re

    return sorted(set(re.findall(r"__[A-Z][A-Z_]*__", text)))


def test_a_file_with_placeholders_is_never_named_plist():
    """The rule that would have prevented this entirely."""
    checked = 0
    for path in sorted(LAUNCHD.iterdir()):
        if not path.is_file():
            continue
        checked += 1
        found = _placeholders(path.read_text())
        if found:
            assert path.name.endswith(".plist.in"), (
                f"{path.name} carries {found} but is named like an installable "
                "plist. Copied into ~/Library/LaunchAgents it registers a job "
                "that can never start, and nothing reports it."
            )

    assert checked, "no launchd files found; this test would pass vacuously"


def test_every_template_is_actually_a_template():
    """The inverse. A `.plist.in` with nothing to substitute is a plist that
    someone renamed, and the installer's substitution step is then a lie."""
    for path in sorted(LAUNCHD.glob("*.plist.in")):
        assert _placeholders(path.read_text()), (
            f"{path.name} is named as a template but has no placeholder to "
            "substitute; either it is not a template or its placeholder was "
            "already filled in and committed"
        )


def test_no_consumer_still_names_the_old_template_path():
    """An installer reading a path that no longer exists stops installing the
    job, and -- as with the original defect -- says nothing."""
    for consumer in CONSUMERS:
        path = REPO_ROOT / consumer
        if not path.exists():
            continue
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            if "launchd/" not in line:
                continue
            after = line.split("launchd/", 1)[1]
            referenced = ""
            for char in after:
                if char.isspace() or char in '"\'':
                    break
                referenced += char
            assert not referenced.endswith(".plist"), (
                f"{consumer}:{number} reads launchd/{referenced}, which no "
                "longer exists; the template is now .plist.in"
            )


def test_every_installer_refuses_an_unsubstituted_plist():
    """Without this check, adding a placeholder to a template silently produces
    a plist that registers and never runs."""
    for installer in INSTALLERS:
        path = REPO_ROOT / installer
        if not path.exists():
            continue
        body = path.read_text()
        assert "__[A-Z_]*__" in body, (
            f"{installer} does not verify that substitution left no placeholder "
            "behind, so a template it does not fully understand would install a "
            "job that can never start"
        )


def test_the_installed_target_is_still_a_plist():
    """Only the TEMPLATE gains the suffix. launchd requires the installed file
    to be a .plist, so renaming that too would break every install."""
    for installer in INSTALLERS:
        path = REPO_ROOT / installer
        if not path.exists():
            continue
        body = path.read_text()
        assert "LaunchAgents" in body
        assert ".plist.in\"" not in body.split("LaunchAgents")[1][:200], (
            f"{installer} appears to install to a .plist.in path; launchd will "
            "not load that"
        )
