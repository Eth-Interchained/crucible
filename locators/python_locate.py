#!/usr/bin/env python3
"""Locate candidate mutation sites in a Python file. Emit JSON on stdout.

WHY THIS IS PYTHON AND NOT RUST
===============================
To mutate Python you must first know exactly where its syntax nodes are. CPython's
own `ast` and `tokenize` modules ARE the grammar that will execute the code — any
third-party reimplementation can disagree with the interpreter about where a node
begins, and a mutation spliced at the wrong offset is a syntax error wearing a
semantic bug's clothes. The forge is Rust; the locator is whatever language owns
the truth about the target's syntax. For `rust/nedb-v2` that will be `syn`.

CONTRACT (stdout, JSON):
    {"file": str, "sha256": str, "candidates": [Candidate, ...],
     "rejected_uncompilable": int, "operators_available": [str, ...]}

Candidate = {
    "operator":     str,   # e.g. "compare_flip"
    "start_byte":   int,   # ABSOLUTE byte offset into the file
    "end_byte":     int,
    "before":       str,   # exact bytes being replaced (decoded)
    "after":        str,   # replacement
    "line":         int,   # 1-based, for humans and for the log
    "context":      str,   # the source line, stripped, for the log
    "scope":        str,   # enclosing def/class, best effort
    "severity":     str,   # "silent" | "loud"  — see SILENT_OPERATORS
}

BYTE OFFSETS, NOT LINE/COL: `ast` and `tokenize` report col_offset as a UTF-8
byte offset within a line, which is a footgun the moment a file has a non-ASCII
character above the mutation site. Resolving to absolute byte offsets HERE, in
the language that knows the file's encoding, means the forge does a pure byte
splice and cannot get it wrong.

EVERY CANDIDATE IS COMPILE-CHECKED BEFORE IT IS EMITTED. A mutation that does not
compile is not a training example — "fix the SyntaxError" teaches nothing about
the defect classes we care about, and it is indistinguishable from a broken
locator. Rejects are COUNTED and reported, never silently dropped.
"""

import ast
import hashlib
import io
import json
import sys
import token as token_mod
import tokenize

# ---------------------------------------------------------------------------
# Operators whose damage is SILENT — the code still runs, still returns, still
# looks right, and lies. This is the defect class that cost a full day on
# 2026-09-08: `except Exception: pass` swallowing a TypeError so that every
# auto INSERT shadow recorded nothing while verify() returned True. A model
# trained to repair these is worth more than one trained on crashes, because
# crashes announce themselves and these do not.
SILENT_OPERATORS = {
    "except_swallow",
    "guard_drop",
    "return_none",
    "compare_flip",
    "boolop_swap",
    "condition_negate",
    "augassign_flip",
    "off_by_one",
    "constant_perturb",
    "await_drop",
}

# Token-level swaps: exact, minimal, and cannot restructure the file.
# Keyed by the token string; value is the replacement.
COMPARE_FLIPS = {
    "<": "<=",
    "<=": "<",
    ">": ">=",
    ">=": ">",
    "==": "!=",
    "!=": "==",
}
BOOLOP_SWAPS = {"and": "or", "or": "and"}
AUGASSIGN_FLIPS = {
    "+=": "-=",
    "-=": "+=",
    "*=": "//=",
    "|=": "&=",
    "&=": "|=",
}
CONSTANT_FLIPS = {"True": "False", "False": "True", "None": "0"}


def _line_starts(data: bytes):
    """Absolute byte offset of the start of each 1-based line."""
    starts = [0, 0]  # index 0 unused; line 1 starts at byte 0
    for i, b in enumerate(data):
        if b == 0x0A:
            starts.append(i + 1)
    return starts


def _abs(line_starts, row: int, col: int) -> int:
    """(1-based row, 0-based UTF-8 byte col) -> absolute byte offset."""
    return line_starts[row] + col


def _compiles(data: bytes, start: int, end: int, after: str, filename: str) -> bool:
    """Does the file still PARSE with this splice applied?

    Uses compile() rather than ast.parse() because compile() also rejects things
    the parser accepts but the compiler does not (e.g. `return` outside a
    function after a guard drop) — which is precisely the class of mutation that
    would otherwise reach the verifier and fail for the wrong reason.
    """
    spliced = data[:start] + after.encode("utf-8") + data[end:]
    try:
        compile(spliced, filename, "exec")
        return True
    except (SyntaxError, ValueError):
        return False


class _ScopeIndex:
    """Maps a line number to its enclosing `def`/`class`, for readable logs."""

    def __init__(self, tree):
        self.spans = []
        for node in ast.walk(tree):
            if isinstance(
                node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                end = getattr(node, "end_lineno", node.lineno)
                self.spans.append((node.lineno, end, node.name))
        # Innermost wins: sort by span width ascending.
        self.spans.sort(key=lambda s: s[1] - s[0])

    def at(self, line: int) -> str:
        for start, end, name in self.spans:
            if start <= line <= end:
                return name
        return "<module>"


def _token_candidates(data, text, line_starts, tree, scopes, filename):
    """Token-level operators. Precise spans, minimal diffs, no restructuring."""
    out, rejected = [], 0
    try:
        toks = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except (tokenize.TokenError, IndentationError, SyntaxError):
        return out, rejected

    lines = text.splitlines()

    for tok in toks:
        ttype, tstr, (srow, scol), (erow, ecol), _ = tok
        if srow != erow:
            continue  # never splice across a line boundary

        table = None
        operator = None
        if ttype == token_mod.OP and tstr in COMPARE_FLIPS:
            table, operator = COMPARE_FLIPS, "compare_flip"
        elif ttype == token_mod.OP and tstr in AUGASSIGN_FLIPS:
            table, operator = AUGASSIGN_FLIPS, "augassign_flip"
        elif ttype == token_mod.NAME and tstr in BOOLOP_SWAPS:
            table, operator = BOOLOP_SWAPS, "boolop_swap"
        elif ttype == token_mod.NAME and tstr in CONSTANT_FLIPS:
            table, operator = CONSTANT_FLIPS, "constant_perturb"
        elif ttype == token_mod.NUMBER:
            # Off-by-one on integer literals only. Floats and complex are left
            # alone: perturbing 0.5 to 1.5 is a different (and usually
            # uninteresting) kind of change than an index being one off.
            try:
                val = int(tstr, 0)
            except ValueError:
                continue
            table = {tstr: str(val + 1)}
            operator = "off_by_one"

        if table is None:
            continue

        after = table[tstr]
        start = _abs(line_starts, srow, scol)
        end = _abs(line_starts, erow, ecol)

        # Paranoia that has earned its place: confirm the bytes we are about to
        # replace are the bytes we think they are. A col_offset mismatch would
        # otherwise corrupt the file quietly and show up as a mystery failure.
        if data[start:end].decode("utf-8", "replace") != tstr:
            rejected += 1
            continue

        if not _compiles(data, start, end, after, filename):
            rejected += 1
            continue

        out.append(
            {
                "operator": operator,
                "start_byte": start,
                "end_byte": end,
                "before": tstr,
                "after": after,
                "line": srow,
                "context": lines[srow - 1].strip() if srow - 1 < len(lines) else "",
                "scope": scopes.at(srow),
                "severity": "silent"
                if operator in SILENT_OPERATORS
                else "loud",
            }
        )
    return out, rejected


def _stmt_span(node, data, line_starts, text):
    """Absolute byte span of a whole statement, including its indentation."""
    lines = text.splitlines(keepends=True)
    start_line = node.lineno
    end_line = getattr(node, "end_lineno", node.lineno)
    start = line_starts[start_line]
    if end_line + 1 < len(line_starts):
        end = line_starts[end_line + 1]
    else:
        end = len(data)
    # Trim the trailing newline so the replacement controls its own line ending.
    while end > start and data[end - 1 : end] == b"\n":
        end -= 1
    indent = ""
    if start_line - 1 < len(lines):
        raw = lines[start_line - 1]
        indent = raw[: len(raw) - len(raw.lstrip())]
    return start, end, indent


def _ast_candidates(data, text, line_starts, tree, scopes, filename):
    """Structural operators. These reshape a statement, so they replace one."""
    out, rejected = [], 0
    lines = text.splitlines()

    def emit(operator, node, after, note_line=None):
        nonlocal rejected
        start, end, indent = _stmt_span(node, data, line_starts, text)
        before = data[start:end].decode("utf-8", "replace")
        replacement = after(indent)
        if replacement == before:
            return
        if not _compiles(data, start, end, replacement, filename):
            rejected += 1
            return
        line = note_line or node.lineno
        out.append(
            {
                "operator": operator,
                "start_byte": start,
                "end_byte": end,
                "before": before,
                "after": replacement,
                "line": line,
                "context": lines[line - 1].strip() if line - 1 < len(lines) else "",
                "scope": scopes.at(line),
                "severity": "silent" if operator in SILENT_OPERATORS else "loud",
            }
        )

    for node in ast.walk(tree):
        # --- except_swallow: turn a handled-and-reported error into silence.
        # THE defect class. `raise`/`return`/`log` inside an except becomes
        # `pass`, so the failure still happens and nothing ever says so.
        if isinstance(node, ast.ExceptHandler):
            body = node.body
            if len(body) == 1 and isinstance(body[0], ast.Pass):
                continue  # already silent; nothing to take away
            inner = body[0]
            # ONLY fire where the handler actually HANDLES. Measured on the
            # first real corpus: this operator hit an `except ImportError:`
            # whose body was `import types as _types, sys as _sys` — fallback
            # SETUP, not error handling. Replacing that with `pass` produces a
            # missing-import bug whose own NameError names the fix, i.e. a
            # copy-paste task mislabelled as silent-failure training. The
            # signature of real handling is raising, returning, or reporting.
            if not isinstance(inner, (ast.Raise, ast.Return)) and not (
                isinstance(inner, ast.Expr) and isinstance(inner.value, ast.Call)
            ):
                continue
            emit(
                "except_swallow",
                inner,
                lambda indent: f"{indent}pass",
                note_line=inner.lineno,
            )

        # --- guard_drop: delete an early-exit guard. The function then runs on
        # input it was explicitly written to refuse.
        if isinstance(node, ast.If) and not node.orelse and len(node.body) == 1:
            only = node.body[0]
            if isinstance(only, (ast.Return, ast.Raise, ast.Continue, ast.Break)):
                emit(
                    "guard_drop",
                    node,
                    lambda indent: f"{indent}pass",
                )

        # --- return_none: keep the signature, drop the value. Callers get None
        # where they expected data, usually far from here.
        if isinstance(node, ast.Return) and node.value is not None:
            if not isinstance(node.value, ast.Constant) or node.value.value is not None:
                emit("return_none", node, lambda indent: f"{indent}return None")

        # --- condition_negate: invert a branch that has both arms, so both
        # halves of the logic run in the wrong circumstances.
        if isinstance(node, ast.If) and node.test is not None:
            t = node.test
            if hasattr(t, "lineno") and t.lineno == getattr(t, "end_lineno", t.lineno):
                start = _abs(line_starts, t.lineno, t.col_offset)
                end = _abs(line_starts, t.end_lineno, t.end_col_offset)
                raw = data[start:end].decode("utf-8", "replace")
                if raw and not raw.startswith("not "):
                    after = f"not ({raw})"
                    if _compiles(data, start, end, after, filename):
                        out.append(
                            {
                                "operator": "condition_negate",
                                "start_byte": start,
                                "end_byte": end,
                                "before": raw,
                                "after": after,
                                "line": t.lineno,
                                "context": lines[t.lineno - 1].strip()
                                if t.lineno - 1 < len(lines)
                                else "",
                                "scope": scopes.at(t.lineno),
                                "severity": "silent",
                            }
                        )
                    else:
                        rejected += 1

        # --- await_drop: remove an await. The coroutine is created and never
        # run; the value becomes a coroutine object nobody awaited.
        if isinstance(node, ast.Await):
            start = _abs(line_starts, node.lineno, node.col_offset)
            inner = node.value
            istart = _abs(line_starts, inner.lineno, inner.col_offset)
            if inner.lineno == node.lineno and istart > start:
                raw = data[start:istart].decode("utf-8", "replace")
                if raw.strip() == "await" and _compiles(
                    data, start, istart, "", filename
                ):
                    out.append(
                        {
                            "operator": "await_drop",
                            "start_byte": start,
                            "end_byte": istart,
                            "before": raw,
                            "after": "",
                            "line": node.lineno,
                            "context": lines[node.lineno - 1].strip()
                            if node.lineno - 1 < len(lines)
                            else "",
                            "scope": scopes.at(node.lineno),
                            "severity": "silent",
                        }
                    )
                else:
                    rejected += 1

    return out, rejected


def locate(path: str):
    with open(path, "rb") as fh:
        data = fh.read()
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as exc:
        return {
            "file": path,
            "error": f"not utf-8: {exc}",
            "candidates": [],
            "rejected_uncompilable": 0,
        }
    try:
        tree = ast.parse(text, filename=path)
    except SyntaxError as exc:
        # A target file that does not parse is a fact about the target, not a
        # crash. Report it and let the forge decide.
        return {
            "file": path,
            "error": f"syntax error at line {exc.lineno}: {exc.msg}",
            "candidates": [],
            "rejected_uncompilable": 0,
        }

    line_starts = _line_starts(data)
    scopes = _ScopeIndex(tree)

    tok_c, tok_r = _token_candidates(data, text, line_starts, tree, scopes, path)
    ast_c, ast_r = _ast_candidates(data, text, line_starts, tree, scopes, path)

    cands = tok_c + ast_c
    # Deterministic order: the same file must always yield the same candidate
    # list in the same order, or a "reproducible" corpus is not one.
    cands.sort(key=lambda c: (c["start_byte"], c["operator"]))

    return {
        "file": path,
        "sha256": hashlib.sha256(data).hexdigest(),
        "candidates": cands,
        "rejected_uncompilable": tok_r + ast_r,
        "operators_available": sorted({c["operator"] for c in cands}),
    }


def main(argv):
    if len(argv) < 2:
        print(
            json.dumps({"error": "usage: python_locate.py FILE [FILE...]"}),
            file=sys.stdout,
        )
        return 2
    results = [locate(p) for p in argv[1:]]
    json.dump(results if len(results) > 1 else results[0], sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
