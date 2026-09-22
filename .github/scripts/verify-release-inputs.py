#!/usr/bin/env python3
import json
import re
import sys
from pathlib import Path


def fail(message: str) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(1)


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"cannot read {path}: {error}")


def read_json(path: Path) -> dict:
    try:
        with path.open(encoding="utf-8") as manifest_file:
            return json.load(manifest_file)
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot parse {path}: {error}")


def required_string(mapping: dict, field_name: str, label: str) -> str:
    value = mapping.get(field_name)
    if not isinstance(value, str):
        fail(f"{label} must contain string {field_name}")
    return value


def required_mapping(mapping: dict, field_name: str, label: str) -> dict:
    value = mapping.get(field_name)
    if not isinstance(value, dict):
        fail(f"{label} must contain object {field_name}")
    return value


def require_file(repository_root: Path, relative_path: str) -> Path:
    path = repository_root / relative_path
    if not path.is_file():
        fail(f"release input is missing: {relative_path}")
    return path


def toml_section(contents: str, section_name: str) -> str:
    section_pattern = re.compile(rf'^\[{re.escape(section_name)}\]\s*$\n?(.*?)(?=^\[|\Z)', re.MULTILINE | re.DOTALL)
    matched_section = section_pattern.search(contents)
    if matched_section is None:
        fail(f"TOML is missing [{section_name}]")
    return matched_section.group(1)


def toml_string(contents: str, section_name: str, field_name: str, label: str) -> str:
    section_contents = toml_section(contents, section_name)
    matched_value = re.search(rf'^\s*{re.escape(field_name)}\s*=\s*"([^"\\]+)"\s*$', section_contents, re.MULTILINE)
    if matched_value is None:
        fail(f"{label} must contain quoted {field_name}")
    return matched_value.group(1)


def cargo_lock_version(lock_contents: str, package_name: str) -> str:
    package_blocks = re.finditer(r'^\[\[package\]\]\s*$\n?(.*?)(?=^\[\[package\]\]|\Z)', lock_contents, re.MULTILINE | re.DOTALL)
    for package_block in package_blocks:
        package_contents = package_block.group(1)
        matched_name = re.search(r'^name\s*=\s*"([^"\\]+)"\s*$', package_contents, re.MULTILINE)
        if matched_name is not None and matched_name.group(1) == package_name:
            matched_version = re.search(r'^version\s*=\s*"([^"\\]+)"\s*$', package_contents, re.MULTILINE)
            if matched_version is None:
                fail(f"Cargo.lock package {package_name} must contain a quoted version")
            return matched_version.group(1)
    fail(f"Cargo.lock is missing package {package_name}")


def ruby_version(path: Path) -> str:
    try:
        contents = path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"cannot read {path}: {error}")
    matched_version = re.search(r'^\s*VERSION\s*=\s*"([^"\\]+)"\s*$', contents, re.MULTILINE)
    if matched_version is None:
        fail("Ruby version file must contain a quoted VERSION assignment")
    return matched_version.group(1)


def openapi_version(path: Path) -> str:
    try:
        contents = path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"cannot read {path}: {error}")
    matched_version = re.search(r'^info:\s*\n(?:(?:^[ \t]+.*\n))*?^[ \t]+version:\s*([^\s#]+)\s*$', contents, re.MULTILINE)
    if matched_version is None:
        fail("OpenAPI document must contain info.version")
    return matched_version.group(1).strip('"\'')


def require_matching_version(observed_version: str, expected_version: str, label: str) -> None:
    if observed_version != expected_version:
        fail(f"{label} version ({observed_version}) does not match v{expected_version}")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: verify-release-inputs.py <repository-root> <release-version>")

    repository_root = Path(sys.argv[1])
    release_version = sys.argv[2]
    cargo_manifest_path = require_file(repository_root, "Cargo.toml")
    cargo_lock_path = require_file(repository_root, "Cargo.lock")
    catalog_manifest_path = require_file(repository_root, "crates/llmshim-catalog/Cargo.toml")
    python_manifest_path = require_file(repository_root, "clients/python/pyproject.toml")
    typescript_manifest_path = require_file(repository_root, "clients/typescript/package.json")
    typescript_lock_path = require_file(repository_root, "clients/typescript/package-lock.json")
    ruby_version_path = require_file(repository_root, "clients/ruby/lib/llmshim/version.rb")
    openapi_path = require_file(repository_root, "api/openapi.yaml")

    cargo_manifest = read_text(cargo_manifest_path)
    cargo_lock = read_text(cargo_lock_path)
    catalog_manifest = read_text(catalog_manifest_path)
    python_manifest = read_text(python_manifest_path)
    typescript_manifest = read_json(typescript_manifest_path)
    typescript_lock = read_json(typescript_lock_path)

    require_matching_version(toml_string(cargo_manifest, "package", "version", "Cargo.toml package"), release_version, "Cargo.toml")
    require_matching_version(cargo_lock_version(cargo_lock, "llmshim"), release_version, "Cargo.lock llmshim")

    catalog_version = toml_string(catalog_manifest, "package", "version", "catalog package")
    catalog_dependency_line = re.search(r'^\s*llmshim-catalog\s*=\s*\{([^}]*)\}\s*$', toml_section(cargo_manifest, "dependencies"), re.MULTILINE)
    if catalog_dependency_line is None:
        fail("Cargo.toml must contain a structured llmshim-catalog dependency")
    catalog_dependency_version = re.search(r'\bversion\s*=\s*"([^"\\]+)"', catalog_dependency_line.group(1))
    if catalog_dependency_version is None:
        fail("llmshim-catalog dependency must contain a quoted version")
    require_matching_version(catalog_dependency_version.group(1), catalog_version, "llmshim-catalog dependency")
    require_matching_version(cargo_lock_version(cargo_lock, "llmshim-catalog"), catalog_version, "Cargo.lock llmshim-catalog")

    python_dynamic_fields = toml_section(python_manifest, "project")
    if re.search(r'^\s*dynamic\s*=\s*\[[^]]*"version"[^]]*\]\s*$', python_dynamic_fields, re.MULTILINE) is None:
        fail("Python package must derive version dynamically")
    if toml_string(python_manifest, "tool.maturin", "manifest-path", "Python maturin config") != "../../Cargo.toml":
        fail("Python package must derive its version from the root Cargo.toml")

    require_matching_version(required_string(typescript_manifest, "version", "TypeScript package.json"), release_version, "TypeScript package.json")
    require_matching_version(required_string(typescript_lock, "version", "TypeScript package-lock.json"), release_version, "TypeScript package-lock.json")
    typescript_lock_packages = required_mapping(typescript_lock, "packages", "TypeScript package-lock.json")
    typescript_lock_root = required_mapping(typescript_lock_packages, "", "TypeScript package-lock.json packages")
    require_matching_version(required_string(typescript_lock_root, "version", "TypeScript package-lock root"), release_version, "TypeScript package-lock root")
    optional_dependencies = required_mapping(typescript_manifest, "optionalDependencies", "TypeScript package.json")
    locked_optional_dependencies = required_mapping(typescript_lock_root, "optionalDependencies", "TypeScript package-lock root")

    platform_packages = {
        "llmshim-darwin-arm64": "llmshim-darwin-arm64",
        "llmshim-darwin-x64": "llmshim-darwin-x64",
        "llmshim-linux-arm64": "llmshim-linux-arm64",
        "llmshim-linux-x64": "llmshim-linux-x64",
        "llmshim-win32-x64": "@sanjay920/llmshim-win32-x64",
    }
    for package_directory, package_name in platform_packages.items():
        platform_manifest_path = require_file(repository_root, f"clients/typescript/packages/{package_directory}/package.json")
        platform_manifest = read_json(platform_manifest_path)
        if required_string(platform_manifest, "name", f"{package_directory} package.json") != package_name:
            fail(f"{package_directory} package name does not match its lockstep dependency")
        require_matching_version(required_string(platform_manifest, "version", f"{package_directory} package.json"), release_version, f"{package_directory} package.json")
        require_matching_version(required_string(optional_dependencies, package_name, "TypeScript optionalDependencies"), release_version, f"TypeScript optional dependency {package_name}")
        require_matching_version(required_string(locked_optional_dependencies, package_name, "TypeScript lock optionalDependencies"), release_version, f"TypeScript lock optional dependency {package_name}")
        resolved_platform_package = typescript_lock_packages.get(f"node_modules/{package_name}")
        if resolved_platform_package is not None:
            if not isinstance(resolved_platform_package, dict):
                fail(f"TypeScript lock resolved entry for {package_name} must be an object")
            resolved_version = resolved_platform_package.get("version")
            if resolved_version is not None:
                if not isinstance(resolved_version, str):
                    fail(f"TypeScript lock resolved version for {package_name} must be a string")
                require_matching_version(resolved_version, release_version, f"TypeScript lock resolved {package_name}")

    require_matching_version(ruby_version(ruby_version_path), release_version, "Ruby client")
    require_matching_version(openapi_version(openapi_path), release_version, "OpenAPI")
    print(f"release inputs for v{release_version} are internally consistent")


if __name__ == "__main__":
    main()
