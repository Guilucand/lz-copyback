#!/usr/bin/env python3
"""Insert #[link_name = "lzcb_<sym>"] before every `pub fn` inside an `extern "C"` block.

The C sources are compiled with lzcb_prefix.h force-included, so every exported symbol
carries the lzcb_ prefix; these attributes are what keeps the Rust-visible names the same.
Re-run after re-vendoring:  python3 tools/add-link-names.py <file>...
"""
import re, sys

FN = re.compile(r'^(\s*)pub fn (\w+)\s*\(')

def process(path):
    out, depth, added = [], 0, 0
    for line in open(path):
        st = line.strip()
        if 'extern "C"' in st and st.endswith('{'):
            depth = 1
        elif depth:
            depth += line.count('{') - line.count('}')
            if depth <= 0:
                depth = 0
        m = FN.match(line)
        if depth and m:
            indent, name = m.groups()
            if not (out and 'link_name' in out[-1]):
                out.append(f'{indent}#[link_name = "lzcb_{name}"]\n')
                added += 1
        out.append(line)
    open(path, 'w').writelines(out)
    print(f"{path}: {added} link_name attributes")

for p in sys.argv[1:]:
    process(p)
