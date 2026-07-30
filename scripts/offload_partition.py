"""Bundled pytest plugin that partitions collected items per Offload group.

Offload discovers tests in a single ``pytest --collect-only`` pass. This plugin
reads a group specification from the file named by ``OFFLOAD_PARTITION_CONFIG``
and, for every collected item, records which groups' filters it satisfies. The
per-group node ID lists are written as JSON to the configured output path.

All filter evaluation delegates to pytest's own matcher machinery, so each
group's members are exactly what pytest itself would select. The plugin makes no
independent selection decisions: it hoists no filters, merges nothing, and
applies no fallback. When ``OFFLOAD_PARTITION_CONFIG`` is unset it does nothing,
leaving ordinary pytest runs untouched.
"""

import fnmatch
import json
import os
from pathlib import Path

import pytest
from _pytest.mark import KeywordMatcher, MarkMatcher
from _pytest.mark.expression import Expression
from _pytest.pathlib import absolutepath

CONFIG_ENV_VAR = "OFFLOAD_PARTITION_CONFIG"


def _compile_expression(expression: str | None) -> Expression | None:
    """Compile a pytest ``-k``/``-m`` expression, or pass through ``None``."""
    if expression is None:
        return None
    else:
        return Expression.compile(expression)


def _mark_matcher(item) -> MarkMatcher:
    """Build the marker matcher pytest uses for ``-m`` against ``item``.

    ``MarkMatcher.from_markers`` exists on pytest >= 8.4; earlier releases only
    expose ``from_item``.
    """
    if hasattr(MarkMatcher, "from_markers"):
        return MarkMatcher.from_markers(item.iter_markers())
    else:
        return MarkMatcher.from_item(item)


def _keep_item(
    item,
    keyword_expr: Expression | None,
    mark_expr: Expression | None,
    deselect_prefixes: tuple[str, ...],
    ignore_paths: list[Path],
    ignore_globs: list[str],
) -> bool:
    """Whether ``item`` survives one group's filters.

    Mirrors pytest's own keyword and mark deselection, ``--deselect`` node ID
    prefix matching, and the path membership and glob checks performed by
    ``pytest_ignore_collect`` at every level of the collection tree.
    """
    if keyword_expr is not None and not keyword_expr.evaluate(
        KeywordMatcher.from_item(item)
    ):
        return False
    if mark_expr is not None and not mark_expr.evaluate(_mark_matcher(item)):
        return False
    if deselect_prefixes and item.nodeid.startswith(deselect_prefixes):
        return False
    collection_paths = [item.path]
    collection_paths.extend(item.path.parents)
    for ignored in ignore_paths:
        if ignored in collection_paths:
            return False
    for glob in ignore_globs:
        for candidate in collection_paths:
            if fnmatch.fnmatch(str(candidate), glob):
                return False
    return True


def _select_group(items, spec: dict[str, object]) -> list[str]:
    """Node IDs from ``items`` that satisfy one group ``spec``, in order."""
    keyword_expr = _compile_expression(spec.get("keyword"))
    mark_expr = _compile_expression(spec.get("mark"))
    deselect_prefixes = tuple(spec.get("deselect") or ())
    ignore_paths = [absolutepath(entry) for entry in spec.get("ignore") or ()]
    ignore_globs = [str(absolutepath(entry)) for entry in spec.get("ignore_glob") or ()]
    selected = []
    for item in items:
        if _keep_item(
            item,
            keyword_expr,
            mark_expr,
            deselect_prefixes,
            ignore_paths,
            ignore_globs,
        ):
            selected.append(item.nodeid)
    return selected


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(items) -> None:
    config_path = os.environ.get(CONFIG_ENV_VAR)
    if config_path is None:
        return
    config = json.loads(Path(config_path).read_text())
    partitions = {
        name: _select_group(items, spec) for name, spec in config["groups"].items()
    }
    Path(config["out"]).write_text(json.dumps({"groups": partitions}))
