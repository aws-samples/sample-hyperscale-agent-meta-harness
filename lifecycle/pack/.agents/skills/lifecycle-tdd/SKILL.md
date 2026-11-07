---
name: lifecycle-tdd
description: Build each behavior test first and keep the failing run. Use for every new behavior and every bug fix, before writing the code that implements it. Covers theory-first bug fixing, the red record in .ham/lifecycle/red, the commit shape, and the rule never to weaken a test.
---
# Test first, record the red, then the code

The record paths below (.ham/lifecycle, .ham/research, .ham/context) follow `.ham/records` (lifecycle-loop): a folder takes the place of .ham, and `off` means write no records.

1. For a defect: reproduce it by hand, then write the theory: what is broken and why, how a test will show it, the data it
   expects, and two other explanations. Fix the cause, not the symptom.
2. Write one failing test for one behavior. Use real inputs: real config files, captured output of the other program, and
   doubles that reject what the real service rejects (formats, enums, required fields). A test helper or fixture file counts
   as part of the test.
3. Run it now, before any other edit, with the repository's own test command (for example pytest). This is the red run. It
   must fail for the stated reason: an assertion, or a missing name the code under test will provide. A compile error
   elsewhere, a missing credential or a broken runner is not a red run. A run made after you changed code for this
   behavior is not a red run, whatever it shows.
4. Save that run in .ham/lifecycle/red/NN-slug.txt, three lines copied from that one run:
   command: <the command you ran>
   exit: <its non-zero exit code>
   failure: <the failing line, copied from the output>
   A red record is added only with its test.
5. Commit the test and the red record alone, as "test: <behavior> (red first)", before any code. The commit holds no code
   files. A script that checks a contract is a test (scripts/check-contract.sh): it goes in this commit. A red record
   committed with code does not count; if it is wrong, correct the file before you commit.
6. Write the smallest code that passes. Run the focused test, then commit as "<area>: <behavior>".
7. Refactor only while green, and run the tests again. Repeat from step 2 for the next behavior, and cover empty, boundary,
   invalid and error cases as well as the normal case.
8. Never edit a test to get green, delete one, add a skip, an expected-failure mark or the like, or lower a threshold. If a
   test is wrong, say why and fix it alone in a commit whose subject starts "test-fix:".
9. If you changed code before the red run, say so in your last message. You can still make a true red run on the original
   code: add a clean worktree of the base commit outside the repository (git worktree add), copy only the test files into
   it, run the test there and save that run. Then commit just the test files and the record in your own tree, before the
   code. If you cannot, write no red record. Never write a red record for a run you did not make.
10. When the whole suite is green, go to lifecycle-build-test.
