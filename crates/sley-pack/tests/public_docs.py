"""Check the SHA-1 admission warning on the rendered public API fields."""
from pathlib import Path
import sys

root = Path(sys.argv[1])
for page, field in [
    ("resolved/struct.ResolvedObject.html", "oid"),
    ("struct.PackIndexBuild.html", "index"),
]:
    html = (root / page).read_text(encoding="utf-8")
    start = html.index(f'id="structfield.{field}"')
    end = html.index('<span id="structfield.', start + 1)
    section = html[start:end]
    assert "sha1dc" in section, f"{page}::{field}: missing collision-detection warning"
    assert "SHA-1" in section, f"{page}::{field}: missing hash format"
    print(f"public docs: {page}::{field} identifies plain SHA-1 and caller sha1dc requirement")
