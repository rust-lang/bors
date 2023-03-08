#!/bin/bash

set -e

sea-orm-cli migrate -d database/migration
sea-orm-cli generate entity -o database/entity/src
