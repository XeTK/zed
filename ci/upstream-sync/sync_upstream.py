#!/usr/bin/env python3
"""Keep the fork in sync with upstream zed-industries/zed `main` -- via a PR, never a merge.

Run daily by .gitea/workflows/sync-upstream.yml (also runnable by hand). Policy:
this script NEVER merges anything into the base branch. It only prepares a
rolling bot-owned branch and opens/updates a pull request; a maintainer merges
it like any other PR.

What it does, each run:
  1. Fetch upstream `main` and the fork's base branch. Nothing new upstream ->
     exit quietly.
  2. Build the rolling branch (default `sync/upstream-main`) = base + a merge
     of upstream/main.
       * Clean merge  -> force-push the branch (it is bot-owned) and create or
         update the single sync PR. If a previous "sync blocked" issue is
         open, close it.
       * Conflicts    -> push NOTHING. Open (or update) one tracking issue that
         lists the conflicting files, so a human notices. The existing PR, if
         any, is left as it was.
  3. Idempotent: if the branch already holds exactly what this run would
     produce, nothing is pushed and no API write is made.

Config (environment):
  GITEA_API_URL   default https://gitea.xetk.co.uk/api/v1
  GITEA_REPO      owner/name, default $GITHUB_REPOSITORY or xetk/zed
  GITEA_TOKEN     token with repo write (contents, pulls, issues). Falls back
                  to $GITHUB_TOKEN, the one the runner injects.
  UPSTREAM_URL    default https://github.com/zed-industries/zed.git
  UPSTREAM_BRANCH default main
  BASE_BRANCH     default main
  SYNC_BRANCH     default sync/upstream-main
  BASE_REMOTE     git remote that points at this repo, default origin (what
                  actions/checkout sets up; use "gitea" in a local checkout)
  DRY_RUN         "1"/"true": read-only; print what would happen, write nothing
                  (no push, no PR, no issue). The merge is still attempted
                  locally so conflicts are reported.
"""
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

API = os.environ.get("GITEA_API_URL", "https://gitea.xetk.co.uk/api/v1").rstrip("/")
REPO = os.environ.get("GITEA_REPO") or os.environ.get("GITHUB_REPOSITORY") or "xetk/zed"
TOKEN = os.environ.get("GITEA_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
UPSTREAM_URL = os.environ.get("UPSTREAM_URL", "https://github.com/zed-industries/zed.git")
UPSTREAM_BRANCH = os.environ.get("UPSTREAM_BRANCH", "main")
BASE = os.environ.get("BASE_BRANCH", "main")
BRANCH = os.environ.get("SYNC_BRANCH", "sync/upstream-main")
DRY = os.environ.get("DRY_RUN", "").lower() in ("1", "true", "yes")
ISSUE_TITLE = "Upstream sync blocked: merge conflicts with zed-industries/zed main"
REMOTE = os.environ.get("BASE_REMOTE", "origin")


def git(*args, check=True, input=None):
    r = subprocess.run(["git", *args], capture_output=True, text=True, input=input)
    if check and r.returncode != 0:
        sys.exit(f"git {' '.join(args)} failed:\n{r.stdout}{r.stderr}")
    return r


def out(*args):
    return git(*args).stdout.strip()


def api(method, path, body=None, ok_404=False):
    if not TOKEN:
        sys.exit("error: no GITEA_TOKEN / GITHUB_TOKEN available")
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"{API}{path}", data=data, method=method)
    req.add_header("Authorization", f"token {TOKEN}")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        if ok_404 and e.code == 404:
            return None
        sys.exit(f"error: {method} {path} -> HTTP {e.code}: {e.read()[:300]!r}")


def say(msg):
    print(msg, flush=True)


def write(desc):
    """Gate for every side effect. Returns True when it is OK to perform it."""
    if DRY:
        say(f"[dry-run] would: {desc}")
        return False
    say(f"doing: {desc}")
    return True


def find_sync_pr():
    for pr in api("GET", f"/repos/{REPO}/pulls?state=open&limit=50") or []:
        if pr["head"]["ref"] == BRANCH and pr["base"]["ref"] == BASE:
            return pr
    return None


def find_blocked_issue():
    for it in api("GET", f"/repos/{REPO}/issues?state=open&type=issues&limit=50") or []:
        if it["title"] == ISSUE_TITLE:
            return it
    return None


def main():
    git("config", "user.name", "zed-sync-bot")
    git("config", "user.email", "zed-sync-bot@users.noreply.gitea.xetk.co.uk")
    if "upstream" not in out("remote").split():
        git("remote", "add", "upstream", UPSTREAM_URL)
    git("fetch", "--no-tags", "upstream", UPSTREAM_BRANCH)
    git("fetch", "--no-tags", REMOTE, BASE)
    up, base = f"upstream/{UPSTREAM_BRANCH}", f"{REMOTE}/{BASE}"

    behind = int(out("rev-list", "--count", f"{base}..{up}"))
    if behind == 0:
        say(f"{BASE} already contains {up}; nothing to sync.")
        return 0
    non_merge = int(out("rev-list", "--count", "--no-merges", f"{base}..{up}"))
    say(f"{base} is missing {behind} upstream commit(s) ({non_merge} non-merge).")

    # Build the candidate merge on a throwaway local branch.
    git("checkout", "-q", "-B", BRANCH, base)
    merged = git("merge", "--no-edit", "--no-ff", "-m",
                 f"Merge {up} into {BRANCH}\n\nAutomated daily upstream sync "
                 f"({behind} commits).", up, check=False)

    if merged.returncode != 0:
        files = [f for f in out("diff", "--name-only", "--diff-filter=U").split("\n") if f]
        git("merge", "--abort", check=False)
        say(f"merge conflicts in {len(files)} file(s):\n  " + "\n  ".join(files))
        body = (f"The daily sync of `zed-industries/zed` `{UPSTREAM_BRANCH}` into `{BASE}` "
                f"cannot merge cleanly. **{behind}** upstream commit(s) are pending.\n\n"
                f"Conflicting files ({len(files)}):\n\n"
                + "\n".join(f"- `{f}`" for f in files)
                + "\n\nNothing was pushed. Resolve by hand (merge the upstream branch "
                  "into a branch off `main`, fix the conflicts, open a PR); this issue "
                  "is updated on every run and closed automatically once a clean merge "
                  "is possible again.\n\n_Opened by ci/upstream-sync/sync_upstream.py._")
        issue = find_blocked_issue() if TOKEN else None
        if issue:
            if issue["body"] != body and write(f"update issue #{issue['number']}"):
                api("PATCH", f"/repos/{REPO}/issues/{issue['number']}", {"body": body})
        elif write("open 'sync blocked' issue"):
            api("POST", f"/repos/{REPO}/issues", {"title": ISSUE_TITLE, "body": body})
        return 0  # a conflict is reported, not a CI failure

    tree = out("rev-parse", "HEAD^{tree}")
    new_tip = out("rev-parse", "HEAD")
    remote_ref = f"{REMOTE}/{BRANCH}"
    git("fetch", "--no-tags", REMOTE, BRANCH, check=False)
    have = git("rev-parse", "--verify", "-q", f"{remote_ref}^{{tree}}", check=False)
    unchanged = have.returncode == 0 and have.stdout.strip() == tree \
        and git("merge-base", "--is-ancestor", base, remote_ref, check=False).returncode == 0
    if unchanged:
        say(f"{BRANCH} already holds this exact merge; nothing to push.")
    elif write(f"force-push {BRANCH} -> {new_tip[:10]}"):
        git("push", "--force-with-lease" if have.returncode == 0 else "--force",
            REMOTE, f"{BRANCH}:{BRANCH}")

    title = f"Merge upstream zed-industries/zed {UPSTREAM_BRANCH} ({behind} commits)"
    body = (f"Automated daily sync of `zed-industries/zed` `{UPSTREAM_BRANCH}` into `{BASE}`.\n\n"
            f"- Upstream commits not yet in `{BASE}`: **{behind}** ({non_merge} non-merge)\n"
            f"- Upstream tip: `{out('rev-parse', '--short=10', up)}`\n"
            f"- Merges cleanly: no conflicts\n\n"
            "**This PR is never merged automatically.** Review the CI run, then merge it "
            "yourself; merging publishes a new nightly. The branch is bot-owned and "
            "rebuilt on every run; do not push to it.\n\n"
            "_Opened by ci/upstream-sync/sync_upstream.py (.gitea/workflows/sync-upstream.yml)._\n\n"
            "Release Notes:\n\n- N/A")
    pr = find_sync_pr() if TOKEN else None
    if pr is None:
        if write(f"open PR '{title}'"):
            made = api("POST", f"/repos/{REPO}/pulls",
                       {"head": BRANCH, "base": BASE, "title": title, "body": body})
            say(f"opened PR #{made['number']}")
    elif (pr["title"], pr["body"]) != (title, body) and \
            write(f"update PR #{pr['number']} title/body"):
        api("PATCH", f"/repos/{REPO}/pulls/{pr['number']}", {"title": title, "body": body})

    issue = find_blocked_issue() if TOKEN else None
    if issue and write(f"close resolved issue #{issue['number']}"):
        api("POST", f"/repos/{REPO}/issues/{issue['number']}/comments",
            {"body": "A clean merge is possible again; closing."})
        api("PATCH", f"/repos/{REPO}/issues/{issue['number']}", {"state": "closed"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
