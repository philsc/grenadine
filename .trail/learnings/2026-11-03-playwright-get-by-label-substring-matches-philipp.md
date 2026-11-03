---
title: Playwright get_by_label substring-matches; new labels can shadow existing locators
author: Philipp
date: 2026-11-03
branch: grenadine/agent/e240755c
id: l-c58e1ae9
scope: repo
status: active
src: agent
touches:
  - grenadine/testing/ui_test.py
---

get_by_label in ui_test.py matches substrings, so a new labelled control can shadow an existing locator: the repo settings dialog's 'Claude permissions'/'Devin permissions' selects made get_by_label('Permissions') a strict-mode violation whenever a settings <dialog> element was still in the DOM (it also lingers briefly after Escape closes it). Match form controls with exact=True when a new label contains their name, and wait for locator('dialog') count 0 after navigating away from a dialog.
