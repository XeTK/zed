#!/usr/bin/env python3
"""Integration test for sync_upstream.py: real git repos + a mock Gitea API.

    python3 ci/upstream-sync/test_sync_upstream.py

Builds a bare "origin" (the fork) and a bare "upstream" in a temp dir, runs the
real script against them, and checks the side effects recorded by a tiny HTTP
server that stands in for Gitea's pulls/issues endpoints.
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sync_upstream.py")


class MockGitea(BaseHTTPRequestHandler):
    state = {"pulls": [], "issues": [], "calls": []}

    def log_message(self, *a):
        pass

    def _send(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        s = self.state
        s["calls"].append(("GET", self.path))
        if "/pulls" in self.path:
            return self._send(s["pulls"])
        if "/issues" in self.path:
            return self._send([i for i in s["issues"] if i["state"] == "open"])
        self._send({}, 404)

    def do_POST(self):
        s, b = self.state, self._body()
        s["calls"].append(("POST", self.path))
        if self.path.endswith("/pulls"):
            pr = {"number": len(s["pulls"]) + 1, "title": b["title"], "body": b["body"],
                  "head": {"ref": b["head"]}, "base": {"ref": b["base"]}}
            s["pulls"].append(pr)
            return self._send(pr, 201)
        if self.path.endswith("/issues"):
            it = {"number": len(s["issues"]) + 100, "title": b["title"], "body": b["body"], "state": "open"}
            s["issues"].append(it)
            return self._send(it, 201)
        self._send({}, 201)

    def do_PATCH(self):
        s, b = self.state, self._body()
        s["calls"].append(("PATCH", self.path))
        n = int(self.path.rsplit("/", 1)[1])
        for coll in ("pulls", "issues"):
            for it in s[coll]:
                if it["number"] == n and f"/{coll}/" in self.path:
                    it.update(b)
        self._send({})


def sh(cwd, *cmd):
    subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True)


def commit(repo, name, text, msg):
    with open(os.path.join(repo, name), "w") as f:
        f.write(text)
    sh(repo, "git", "add", name)
    sh(repo, "git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", msg)


class SyncTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        t = self.tmp
        self.up_bare, self.fork_bare = f"{t}/up.git", f"{t}/fork.git"
        self.up, self.fork = f"{t}/up", f"{t}/fork"
        for b in (self.up_bare, self.fork_bare):
            sh(t, "git", "init", "-q", "--bare", "-b", "main", b)
        sh(t, "git", "clone", "-q", self.up_bare, self.up)
        commit(self.up, "shared.txt", "one\n", "base")
        commit(self.up, "other.txt", "o\n", "base2")
        sh(self.up, "git", "push", "-q", "origin", "HEAD:main")
        sh(t, "git", "clone", "-q", self.up_bare, self.fork)
        sh(self.fork, "git", "remote", "set-url", "origin", self.fork_bare)
        sh(self.fork, "git", "push", "-q", "origin", "HEAD:main")
        MockGitea.state.update(pulls=[], issues=[], calls=[])
        self.srv = HTTPServer(("127.0.0.1", 0), MockGitea)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def tearDown(self):
        self.srv.shutdown()

    def run_sync(self, dry=False):
        env = dict(os.environ, GITEA_API_URL=f"http://127.0.0.1:{self.srv.server_port}",
                   GITEA_REPO="x/y", GITEA_TOKEN="t", UPSTREAM_URL=self.up_bare,
                   DRY_RUN="1" if dry else "")
        r = subprocess.run([sys.executable, SCRIPT], cwd=self.fork, env=env,
                           capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        return r.stdout

    def writes(self):
        return [c for c in MockGitea.state["calls"] if c[0] != "GET"]

    def fork_has(self, ref):
        return subprocess.run(["git", "rev-parse", "--verify", "-q", ref], cwd=self.fork_bare,
                              capture_output=True).returncode == 0

    def upstream_advances(self, name="new.txt"):
        commit(self.up, name, "n\n", f"add {name}")
        sh(self.up, "git", "push", "-q", "origin", "HEAD:main")

    def test_up_to_date_does_nothing(self):
        out = self.run_sync()
        self.assertIn("nothing to sync", out)
        self.assertEqual(self.writes(), [])

    def test_clean_sync_pushes_branch_and_opens_one_pr_never_merges(self):
        commit(self.fork, "fork_only.txt", "f\n", "fork change")
        sh(self.fork, "git", "push", "-q", "origin", "HEAD:main")
        self.upstream_advances()
        self.run_sync()
        self.assertTrue(self.fork_has("refs/heads/sync/upstream-main"))
        self.assertEqual(len(MockGitea.state["pulls"]), 1)
        self.assertEqual(MockGitea.state["pulls"][0]["base"]["ref"], "main")
        self.assertIn("never merged automatically", MockGitea.state["pulls"][0]["body"])
        self.assertFalse(any("merge" in p for _, p in self.writes()), "must never call a merge endpoint")
        # main on the fork is untouched (the sync branch is ahead of it, not the reverse)
        sh(self.fork_bare, "git", "merge-base", "--is-ancestor", "main", "sync/upstream-main")
        up_tip = subprocess.run(["git", "rev-parse", "main"], cwd=self.up_bare, capture_output=True, text=True).stdout
        fork_main = subprocess.run(["git", "rev-parse", "main"], cwd=self.fork_bare, capture_output=True, text=True).stdout
        self.assertNotEqual(fork_main, up_tip, "fork main must not have been advanced to upstream")

    def test_rerun_is_idempotent(self):
        self.upstream_advances()
        self.run_sync()
        MockGitea.state["calls"].clear()
        out = self.run_sync()
        self.assertIn("already holds this exact merge", out)
        self.assertEqual(self.writes(), [], "second run with no change must write nothing")
        self.assertEqual(len(MockGitea.state["pulls"]), 1)

    def test_more_upstream_commits_update_the_same_pr(self):
        self.upstream_advances("a.txt")
        self.run_sync()
        self.upstream_advances("b.txt")
        self.run_sync()
        self.assertEqual(len(MockGitea.state["pulls"]), 1, "must update, not open a second PR")
        self.assertIn("2 commits", MockGitea.state["pulls"][0]["title"])

    def test_conflict_pushes_nothing_and_opens_one_issue(self):
        commit(self.fork, "shared.txt", "fork\n", "fork edit")
        sh(self.fork, "git", "push", "-q", "origin", "HEAD:main")
        commit(self.up, "shared.txt", "upstream\n", "upstream edit")
        sh(self.up, "git", "push", "-q", "origin", "HEAD:main")
        self.run_sync()
        self.run_sync()  # second run must not duplicate the issue
        self.assertFalse(self.fork_has("refs/heads/sync/upstream-main"))
        self.assertEqual(MockGitea.state["pulls"], [])
        self.assertEqual(len(MockGitea.state["issues"]), 1)
        self.assertIn("shared.txt", MockGitea.state["issues"][0]["body"])

    def test_issue_closes_when_conflict_clears(self):
        MockGitea.state["issues"].append({"number": 100, "state": "open", "body": "x",
                                          "title": "Upstream sync blocked: merge conflicts with zed-industries/zed main"})
        self.upstream_advances()
        self.run_sync()
        self.assertEqual(MockGitea.state["issues"][0]["state"], "closed")

    def test_dry_run_writes_nothing(self):
        self.upstream_advances()
        self.run_sync(dry=True)
        self.assertEqual(self.writes(), [])
        self.assertFalse(self.fork_has("refs/heads/sync/upstream-main"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
