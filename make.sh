#!/bin/bash
set -e
#/*%LPH%*/

pgv=$(pg_config --version)
v=${pgv:11:2}
echo "Building trigger extension for Postgres# $v"
psql -c "drop extension if exists rppd;" || true

cargo build --release -F pg$v
#cargo build --lib --release -F pg$v

cargo pgrx package

sudo cp target/release/rppd-pg$v/usr/share/postgresql/$v/extension/rppd* /usr/share/postgresql/$v/extension/
sudo cp target/release/rppd-pg$v/usr/lib/postgresql/$v/lib/rppd.so /usr/lib/postgresql/$v/lib

psql -c "create extension if not exists rppd" || true
psql -c "\d rppd_config"

#cargo build --bin rppd --release -F pg$v

echo "Release build. use target/release/rppd to run"