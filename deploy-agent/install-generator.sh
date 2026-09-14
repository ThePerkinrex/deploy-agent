#!/bin/bash

sudo mkdir -p /etc/systemd/system-generators
sudo install -o root -g root -m 0755 deploy-agent-generator.sh /etc/systemd/system-generators/deploy-agent-generator