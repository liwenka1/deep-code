---
name: review
description: Review a commit range of this repository the way its own review rounds do — verify claims against code, prove each fix with a mutation, and report by severity.
---

# Review

Scope: the commits named, or `origin/main..HEAD` when none are. Read the code,
not the commit messages — the messages are claims to verify.

## Method

1. For every claim a commit message or doc makes about behaviour, find the
   test that pins it. A claim with no test is a finding on its own, and a
   security claim with no test is a blocker.
2. A test pins a claim only at the right layer: "the user sees X" is proven on
   rendered terminal cells (`TestBackend`), not on a `Vec<Line>`; "the model
   is told X" is proven on the wire messages, not on a helper's return value.
3. For each fix, name the mutation that would make its test fail, and run it:
   revert the one line, run the test, confirm it goes red, restore. A fix whose
   test stays green under mutation is not fixed.
4. When a reviewer (human or agent) reports a P0, reproduce it yourself before
   acting; a confident report is not evidence.
5. Fix one call site, then count its siblings: the same read spelled elsewhere
   is the same bug. This holds for prose too — grep every restatement before
   rewording one.
6. Platform-gated code (`cfg(windows)`, `cfg(target_os = "linux")`) cannot be
   run here; read it line by line and say it was not executed.

## Report

Order by severity (P0 blocks a release; P1 ships only with a note; P2/P3 are
batched). For each finding give the file and line, what breaks, the input that
triggers it, and the smallest fix. Separate "the finding is real" from "the
proposed fix is right" — both are verified independently. End with whether
the range can ship, in one sentence.
