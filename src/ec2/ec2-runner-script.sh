#!/bin/bash

# $JITCONFIG is replaced by bors with the actual JIT token.
echo '$JITCONFIG' | sudo -u ubuntu tee /home/ubuntu/jit-token > /dev/null

# Will terminate instance once it exits (either successfully or with failure).
# The unit itself is defined as part of the pre-built AMI, see
# https://github.com/rust-lang/simpleinfra/blob/master/terragrunt/modules/ci-runners/lambda/ubuntu.pkr.hcl
systemctl start gha-runner
