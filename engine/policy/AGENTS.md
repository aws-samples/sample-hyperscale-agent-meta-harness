# engine/policy

## Purpose

Holds `image-key.pin`, the pinned worker image key: the value that `scripts/image-key-guard.sh` compares with the key
computed from the committed tree. It also holds `cdk-bootstrap-template.pin`, the pin of the CDK bootstrap template.

## Ownership

Owned by `engine/AGENTS.md`. Only `scripts/image-key-guard.sh --update` writes the pin; `scripts/lib/image_key_guard.py` reads it.

## Local Contracts

- `image-key.pin` is `#` comment lines and one line of 64 lowercase hex digits. Never edit it by hand.
- The key is `scripts/lib/digests.py` `worker_image_key`: the same value `scripts/lib/deploy.py` tags the worker image with. It hashes the
  crate closure of `agentd`, the Cargo files, `accounts.json`, `engine/` outside `engine/catalog/` and `engine/policy/`, and the image
  sources. `IMAGE_EXCLUDED` keeps this directory out of the key, so rewriting the pin never moves it.
- A commit that changes the key on purpose updates the pin in the same commit: `git add` the change, run `scripts/image-key-guard.sh
  --update`, `git add engine/policy/image-key.pin`, commit. Any other commit leaves the key equal to the pin.

- `cdk-bootstrap-template.pin` is `#` comment lines and two lines, `awsCdk=<version>` and `sha256=<64 hex digits>`: the SHA-256 of the output of `cdk bootstrap --show-template` of that
  aws-cdk version. A fresh account has no CDK bootstrap; the runner prints the template with the pinned CDK CLI and `setup.sh` deploys it with the user's credentials only when its
  SHA-256 is this one. `awsCdk` is the version of `tools.lock` ([node] aws_cdk); a bump of the CDK changes the pin in the same change, proved by the live case of
  `scripts/tests/test_cdk_bootstrap_pin.py` (`HAMH_BUILD_TESTS=1` on a build host with Docker). Unlike `image-key.pin`, this file is edited by hand, because nothing computes it from the tree.
  `IMAGE_EXCLUDED` keeps this directory out of the worker image key.

## Work Guidance

## Verification

```bash
scripts/image-key-guard.sh   # IMAGE-KEY PASS <key>
python3 -m unittest discover -s scripts/tests -p test_cdk_bootstrap_pin.py   # the pin of the bootstrap template; HAMH_BUILD_TESTS=1 adds the live case
```

## Child DOX Index

None.
