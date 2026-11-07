#!/usr/bin/env bash
# image-size-gate.sh <unpacked|compressed> <bytes> <ceiling>: the image-size gate of images/codebuild/worker-python.yml.
#   compressed  <bytes> is ECR's imageSizeInBytes for the pushed tag and the ceiling is in MB (1 MB = 1,000,000 bytes). The platform counts these bytes,
#               so this is the gate that matters; the build spec runs it right after the push.
#   unpacked    <bytes> is `docker image inspect --format '{{.Size}}'` and the ceiling is in MiB (1 MiB = 1,048,576 bytes). It is a loose ceiling that keeps
#               growth visible; the build spec runs it before the push, so an image far above it is never pushed.
# Prints IMAGE-GATE PASS <kind> <size> <unit> (ceiling <n> <unit>) and exits 0, or IMAGE-GATE FAIL ... and exits 1. A size that is not a positive whole
# number (an empty value, None from `aws --output text`, 0) fails: a gate that cannot measure does not pass. A bad call exits 2.
set -euo pipefail
if [ $# -ne 3 ]; then
  echo "usage: image-size-gate.sh <unpacked|compressed> <bytes> <ceiling>" >&2
  exit 2
fi
kind=$1 bytes=$2 ceiling=$3
case "$kind" in
  unpacked) unit=1048576 name=MiB ;;
  compressed) unit=1000000 name=MB ;;
  *)
    echo "image-size-gate.sh: unknown kind $kind (unpacked or compressed)" >&2
    exit 2
    ;;
esac
case "$ceiling" in '' | *[!0-9]* | 0*)
  echo "image-size-gate.sh: the ceiling must be a positive whole number, got '$ceiling'" >&2
  exit 2
  ;;
esac
if [ "${#ceiling}" -gt 9 ]; then
  echo "image-size-gate.sh: the ceiling '$ceiling' is not a plausible size" >&2
  exit 2
fi
case "$bytes" in '' | *[!0-9]* | 0*)
  echo "IMAGE-GATE FAIL $kind: no size to compare (got '$bytes'; ceiling $ceiling $name)"
  exit 1
  ;;
esac
if [ "${#bytes}" -gt 15 ]; then
  echo "IMAGE-GATE FAIL $kind: the size '$bytes' is not plausible (ceiling $ceiling $name)"
  exit 1
fi
size=$((bytes / unit))
if [ "$bytes" -le $((ceiling * unit)) ]; then
  echo "IMAGE-GATE PASS $kind $size $name (ceiling $ceiling $name)"
else
  echo "IMAGE-GATE FAIL $kind $bytes bytes ($size $name) is over the ceiling of $ceiling $name ($((ceiling * unit)) bytes)"
  exit 1
fi
