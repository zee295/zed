#!/usr/bin/env python3
import re
from pathlib import Path

root = Path(__file__).parent / "crates"

for path in root.rglob("*.rs"):
    text = path.read_text()
    orig = text
    # Simple fully-qualified now()
    text = text.replace("std::time::Instant::now()", "web_time::Instant::now()")
    # Simple import lines
    text = text.replace("use std::time::Instant;", "use web_time::Instant;")
    text = text.replace("use std::time::{Duration, Instant};", "use web_time::{Duration, Instant};")
    # Block import inside std::{ ... time::{Duration, Instant}, ... }
    if "time::{Duration, Instant}," in text:
        text = text.replace("time::{Duration, Instant},", "time::Duration,")
        if "web_time::Instant" not in text:
            # Insert after first 'use ' line
            lines = text.splitlines(keepends=True)
            idx = 0
            for i, line in enumerate(lines):
                if line.startswith("use "):
                    idx = i
                    break
            lines.insert(idx, "use web_time::Instant;\n")
            text = "".join(lines)
    if text != orig:
        path.write_text(text)
        print("updated", path)
