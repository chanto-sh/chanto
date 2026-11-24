#!/usr/bin/env python3
"""Generate the SDKs' types from sdk/schema.json.

    python3 sdk/gen_types.py           # write typescript/types.ts and python/chanto_types.py
    python3 sdk/gen_types.py --check   # fail if either is out of date (CI)

sdk/schema.json comes from the server's own wire types (src/wire.rs), so the
types here describe exactly what `chanto serve` sends and accepts. It covers
the subset of JSON Schema that schemars writes for them: objects, string
enums, constants, nullable fields, arrays, references and the tagged union
of stream events. Standard library only, like the SDKs.
"""
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCHEMA = HERE / "schema.json"
TS_OUT = HERE / "typescript" / "types.ts"
PY_OUT = HERE / "python" / "chanto_types.py"
HEADER = "Generated from sdk/schema.json by sdk/gen_types.py: don't edit by hand."


def pascal(name):
    return "".join(part[:1].upper() + part[1:] for part in re.split(r"[_\W]+", name) if part)


def ref_name(ref):
    return ref.rsplit("/", 1)[-1]


def variants(defs):
    """The stream events: (type name, its `type` tag, the schema) for each."""
    out = []
    for v in defs["StreamEvent"]["oneOf"]:
        tag = v["properties"]["type"]["const"]
        out.append((pascal(tag) + "Event", tag, v))
    return out


def fields(schema, defs):
    """(name, schema, required) for an object, merging a referenced base."""
    props = dict(schema.get("properties", {}))
    required = set(schema.get("required", []))
    if "$ref" in schema:
        base = defs[ref_name(schema["$ref"])]
        props = {**base.get("properties", {}), **props}
        required |= set(base.get("required", []))
    return [(name, s, name in required) for name, s in props.items()]


def types_of(schema):
    t = schema.get("type")
    return t if isinstance(t, list) else [t] if t else []


# TypeScript

def ts_type(s):
    if s is True:
        return "unknown"
    if "$ref" in s:
        return ref_name(s["$ref"])
    if "const" in s:
        return json.dumps(s["const"])
    if "enum" in s:
        return " | ".join(json.dumps(e) for e in s["enum"])
    parts = []
    for t in types_of(s):
        parts.append({"string": "string", "integer": "number", "number": "number", "boolean": "boolean",
                      "null": "null", "object": "Record<string, unknown>"}.get(t)
                     or f"{ts_type(s.get('items', True))}[]")
    if "anyOf" in s:
        parts += [ts_type(x) for x in s["anyOf"]]
    return " | ".join(parts) or "unknown"


def ts_doc(text, indent=""):
    if not text:
        return []
    lines = text.splitlines()
    if len(lines) == 1:
        return [f"{indent}/** {lines[0]} */"]
    return [f"{indent}/**"] + [f"{indent} * {l}".rstrip() for l in lines] + [f"{indent} */"]


def ts_interface(name, schema, defs, extra=()):
    out = ts_doc(schema.get("description")) + [f"export interface {name} {{"]
    for line in extra:
        out.append(f"  {line}")
    for field, s, required in fields(schema, defs):
        if field == "type" and extra:
            continue
        out += ts_doc(s.get("description") if isinstance(s, dict) else None, "  ")
        out.append(f"  {field}{'' if required else '?'}: {ts_type(s)};")
    return out + ["}", ""]


def typescript(defs):
    out = [f"// {HEADER}", ""]
    for name in sorted(defs):
        if name == "StreamEvent":
            continue
        s = defs[name]
        if "enum" in s:
            out += ts_doc(s.get("description")) + [f"export type {name} = {ts_type(s)};", ""]
        else:
            out += ts_interface(name, s, defs)
    names = []
    for name, tag, v in variants(defs):
        names.append(name)
        out += ts_interface(name, v, defs, extra=[f"type: {json.dumps(tag)};"])
    out += ts_doc(defs["StreamEvent"].get("description"))
    out += ["export type StreamEvent ="] + [f"  | {n}" for n in names]
    out[-1] += ";"
    return "\n".join(out) + "\n"


# Python

def py_type(s):
    if s is True:
        return "Any"
    if "$ref" in s:
        return f'"{ref_name(s["$ref"])}"'
    if "const" in s:
        return f"Literal[{json.dumps(s['const'])}]"
    if "enum" in s:
        return f"Literal[{', '.join(json.dumps(e) for e in s['enum'])}]"
    parts = []
    nullable = False
    for t in types_of(s):
        if t == "null":
            nullable = True
            continue
        parts.append({"string": "str", "integer": "int", "number": "float", "boolean": "bool",
                      "object": "Dict[str, Any]"}.get(t) or f"List[{py_type(s.get('items', True))}]")
    if "anyOf" in s:
        parts += [py_type(x) for x in s["anyOf"]]
    t = parts[0] if len(parts) == 1 else f"Union[{', '.join(parts)}]" if parts else "Any"
    return f"Optional[{t}]" if nullable else t


def py_doc(text, indent="    "):
    if not text:
        return []
    lines = text.replace('"""', "'''").splitlines()
    if len(lines) == 1:
        return [f'{indent}"""{lines[0]}"""']
    return [f'{indent}"""{lines[0]}'] + [f"{indent}{l}".rstrip() for l in lines[1:]] + [f'{indent}"""']


def py_typeddict(name, schema, defs, tag=None):
    """A TypedDict; fields that may be left out go in a total=False subclass,
    which works on every Python 3.8+ (NotRequired is 3.11)."""
    all_fields = fields(schema, defs)
    required = [(f, s) for f, s, r in all_fields if r]
    optional = [(f, s) for f, s, r in all_fields if not r]
    if tag is not None:
        required = [("type", {"const": tag})] + [(f, s) for f, s in required if f != "type"]

    def body(items):
        lines = []
        for f, s in items:
            lines.append(f"    {f}: {py_type(s)}")
            lines += py_doc(s.get("description") if isinstance(s, dict) else None)
        return lines or ["    pass"]

    doc = py_doc(schema.get("description"))
    if not optional:
        return [f"class {name}(TypedDict):"] + doc + body(required) + ["", ""]
    return ([f"class _{name}Required(TypedDict):"] + body(required) + ["", ""]
            + [f"class {name}(_{name}Required, total=False):"] + doc + body(optional) + ["", ""])


def python(defs):
    out = [f'"""{HEADER}"""', "",
           "from typing import Any, Dict, List, Optional, Union",
           "",
           "try:",
           "    from typing import Literal, TypedDict",
           "except ImportError:  # Python 3.7",
           "    from typing_extensions import Literal, TypedDict  # type: ignore",
           "", ""]
    for name in sorted(defs):
        if name == "StreamEvent":
            continue
        s = defs[name]
        if "enum" in s:
            out += [f"{name} = {py_type(s)}"] + [f"# {l}" for l in (s.get("description") or "").splitlines()] + ["", ""]
        else:
            out += py_typeddict(name, s, defs)
    names = []
    for name, tag, v in variants(defs):
        names.append(name)
        out += py_typeddict(name, v, defs, tag=tag)
    out += ["StreamEvent = Union["] + [f"    {n}," for n in names] + ["]"]
    out += [f"# {l}" for l in (defs["StreamEvent"].get("description") or "").splitlines()]
    return "\n".join(out) + "\n"


def main():
    defs = json.loads(SCHEMA.read_text())["$defs"]
    outputs = {TS_OUT: typescript(defs), PY_OUT: python(defs)}
    if "--check" in sys.argv[1:]:
        stale = [str(p.relative_to(HERE.parent)) for p, text in outputs.items()
                 if not p.exists() or p.read_text() != text]
        if stale:
            sys.exit(f"out of date: {', '.join(stale)}; run python3 sdk/gen_types.py")
        print("SDK types are current")
        return
    for p, text in outputs.items():
        p.write_text(text)
        print(f"wrote {p.relative_to(HERE.parent)}")


if __name__ == "__main__":
    main()
