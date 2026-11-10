"""Checks the fake GitHub's hand-written GraphQL schema against the real one.

Every type, field, argument, enum value and union member in
github_schema.graphql must exist in GitHub's public schema (pinned in
MODULE.bazel as @github_graphql_schema) with a matching type structure, so a
query the fake accepts isn't rejected by GitHub. Nullability and default
values are not compared: the fake relaxes them on purpose.
"""

import os
import unittest
from pathlib import Path

from graphql import (
    GraphQLList,
    GraphQLNonNull,
    build_schema,
    is_enum_type,
    is_input_object_type,
    is_interface_type,
    is_object_type,
    is_scalar_type,
    is_union_type,
)

FAKE_SCHEMA = build_schema(
    Path(__file__).with_name("github_schema.graphql").read_text()
)
REAL_SCHEMA = build_schema(
    Path(os.environ["GITHUB_GRAPHQL_SCHEMA"]).absolute().read_text()
)

BUILT_IN_SCALARS = {"String", "Int", "Float", "Boolean", "ID"}

# The fake types the real schema doesn't have, each with the reason it's
# allowed to differ.
EXTRA_TYPES = {
    # The fake gives every mutation this one shared payload type; its fields
    # are checked against each real payload type instead.
    "MutationPayload",
}


def kind(graphql_type) -> str:
    for predicate, name in (
        (is_object_type, "object"),
        (is_interface_type, "interface"),
        (is_union_type, "union"),
        (is_enum_type, "enum"),
        (is_input_object_type, "input"),
        (is_scalar_type, "scalar"),
    ):
        if predicate(graphql_type):
            return name
    raise AssertionError(f"unknown kind: {graphql_type}")


def shape(graphql_type):
    """The type structure with NonNull stripped: nested lists down to a name."""
    while isinstance(graphql_type, GraphQLNonNull):
        graphql_type = graphql_type.of_type
    if isinstance(graphql_type, GraphQLList):
        return ("list", shape(graphql_type.of_type))
    return graphql_type.name


def named_type(graphql_type):
    while isinstance(graphql_type, (GraphQLNonNull, GraphQLList)):
        graphql_type = graphql_type.of_type
    return graphql_type


class SchemaTest(unittest.TestCase):
    def test_fake_schema_matches_real_schema(self) -> None:
        mismatches = []

        for type_name, fake_type in sorted(FAKE_SCHEMA.type_map.items()):
            if type_name.startswith("__") or type_name in BUILT_IN_SCALARS:
                continue
            if type_name in EXTRA_TYPES:
                continue
            real_type = REAL_SCHEMA.type_map.get(type_name)
            if real_type is None:
                mismatches.append(f"type {type_name} is not in the real schema")
                continue
            if kind(fake_type) != kind(real_type):
                mismatches.append(
                    f"type {type_name} is a {kind(fake_type)} in the fake"
                    f" but a {kind(real_type)} in the real schema"
                )
                continue

            if is_object_type(fake_type) or is_interface_type(fake_type):
                self._check_fields(
                    type_name, fake_type.fields, real_type.fields, mismatches
                )
            elif is_input_object_type(fake_type):
                for field_name, fake_field in fake_type.fields.items():
                    real_field = real_type.fields.get(field_name)
                    if real_field is None:
                        mismatches.append(
                            f"{type_name}.{field_name} is not in the real schema"
                        )
                    elif shape(fake_field.type) != shape(real_field.type):
                        mismatches.append(
                            f"{type_name}.{field_name} is {shape(fake_field.type)}"
                            f" in the fake but {shape(real_field.type)} in the real schema"
                        )
            elif is_enum_type(fake_type):
                for value in fake_type.values:
                    if value not in real_type.values:
                        mismatches.append(
                            f"enum value {type_name}.{value} is not in the real schema"
                        )
            elif is_union_type(fake_type):
                real_members = {t.name for t in real_type.types}
                for member in fake_type.types:
                    if member.name not in real_members:
                        mismatches.append(
                            f"union member {member.name} is not in the real {type_name}"
                        )

        self.assertEqual(mismatches, [])

    def _check_fields(self, type_name, fake_fields, real_fields, mismatches) -> None:
        for field_name, fake_field in fake_fields.items():
            where = f"{type_name}.{field_name}"
            real_field = real_fields.get(field_name)
            if real_field is None:
                mismatches.append(f"{where} is not in the real schema")
                continue
            fake_shape = shape(fake_field.type)
            if type_name == "Mutation" and fake_shape == "MutationPayload":
                real_payload = named_type(real_field.type)
                for payload_field in FAKE_SCHEMA.type_map["MutationPayload"].fields:
                    if payload_field not in real_payload.fields:
                        mismatches.append(
                            f"{real_payload.name}.{payload_field} is not in the real"
                            f" schema (fake's {where} payload)"
                        )
            elif fake_shape != shape(real_field.type):
                mismatches.append(
                    f"{where} is {fake_shape} in the fake"
                    f" but {shape(real_field.type)} in the real schema"
                )
            for arg_name, fake_arg in fake_field.args.items():
                real_arg = real_field.args.get(arg_name)
                if real_arg is None:
                    mismatches.append(f"{where}({arg_name}:) is not in the real schema")
                elif shape(fake_arg.type) != shape(real_arg.type):
                    mismatches.append(
                        f"{where}({arg_name}:) is {shape(fake_arg.type)}"
                        f" in the fake but {shape(real_arg.type)} in the real schema"
                    )


if __name__ == "__main__":
    unittest.main()
