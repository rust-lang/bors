#!/bin/bash

# JITCONFIG is *textually* replaced by bors with the actual JIT token.
# The JIT token is base64 encoded by GitHub, so it shouldn't have any single quotes in the string.
#
# This is read by the gha-runner service started below.
echo '$JITCONFIG' | sudo -u ubuntu tee /home/ubuntu/jit-token > /dev/null

# Will terminate instance once it exits (either successfully or with failure).
#
# If no job is started in a few minutes after this runs, the instance will also
# terminate.
#
# The unit itself is defined as part of the pre-built AMI, see
# https://github.com/rust-lang/simpleinfra/blob/master/terragrunt/modules/ci-runners/lambda/ubuntu.pkr.hcl
#
# --no-block avoids blocking the remainder of cloud-init on this job starting. That keeps the journal and console tidier.
systemctl start --no-block gha-runner
