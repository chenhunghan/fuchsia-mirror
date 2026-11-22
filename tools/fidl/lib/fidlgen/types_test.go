// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fidlgen

import (
	"encoding/json"
	"math"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
)

// toDocComment formats doc comments in a by adding a leading space and a
// trailing newline.
func toDocComment(input string) string {
	return " " + input + "\n"
}

func TestCanUnmarshalLargeOrdinal(t *testing.T) {
	input := `{
		"ordinal": 18446744073709551615
	}`

	var method Method
	err := json.Unmarshal([]byte(input), &method)
	if err != nil {
		t.Fatalf("failed to unmarshal: %s", err)
	}
	if method.Ordinal != math.MaxUint64 {
		t.Fatalf("method.Ordinal: expected math.MaxUint64, found %d", method.Ordinal)
	}
}

func TestEncodedCompoundIdentifierParsing(t *testing.T) {
	type testCase struct {
		input          EncodedCompoundIdentifier
		expectedOutput CompoundIdentifier
	}
	tests := []testCase{
		{
			input:          "Decl",
			expectedOutput: compoundIdentifier([]string{""}, "Decl", ""),
		},
		{
			input:          "fuchsia.some.library/Decl",
			expectedOutput: compoundIdentifier([]string{"fuchsia", "some", "library"}, "Decl", ""),
		},
		{
			input:          "Name.MEMBER",
			expectedOutput: compoundIdentifier([]string{""}, "Name", "MEMBER"),
		},

		{
			input:          "fuchsia.some.library/Decl.MEMBER",
			expectedOutput: compoundIdentifier([]string{"fuchsia", "some", "library"}, "Decl", "MEMBER"),
		},
	}
	for _, test := range tests {
		output := test.input.Parse()
		diff := cmp.Diff(output, test.expectedOutput)
		if len(diff) > 0 {
			t.Errorf("unexpected output for input %q diff: %s", test.input, diff)
		}
	}
}

func TestExperimentsParsing(t *testing.T) {
	type testCase struct {
		desc     string
		ir       string
		expected Experiments
	}
	tests := []testCase{
		{
			desc:     "empty",
			ir:       `{"experiments": []}`,
			expected: Experiments{},
		},
		{
			desc:     "single",
			ir:       `{"experiments": ["foo"]}`,
			expected: Experiments{"foo"},
		},
		{
			desc:     "multiple",
			ir:       `{"experiments": ["foo", "bar"]}`,
			expected: Experiments{"foo", "bar"},
		},
	}
	for _, test := range tests {
		ir, err := ReadJSONIrContent([]byte(test.ir))
		if err != nil {
			t.Errorf("case (%s) could not parse JSON IR: %v", test.desc, err)
		}

		actual := ir.Experiments
		opt := cmpopts.SortSlices(func(a, b string) bool { return a < b })
		if diff := cmp.Diff(actual, test.expected, opt); len(diff) > 0 {
			t.Errorf("case (%s) \nexpected: %#v\nactual: %#v\n", test.desc, test.expected, actual)
		}

		// Make sure the "Contains" method works.
		for _, ex := range test.expected {
			if !actual.Contains(ex) {
				t.Errorf("case (%s) Contains(\"%s\") returned false for present experiment", test.desc, ex)
			}
		}
		if actual.Contains(Experiment("should always fail")) {
			t.Errorf("case (%s) Contains always returns true", test.desc)
		}
	}
}

func compoundIdentifier(library []string, name, member string) CompoundIdentifier {
	var convertedLibrary LibraryIdentifier
	for _, part := range library {
		convertedLibrary = append(convertedLibrary, Identifier(part))
	}
	return CompoundIdentifier{
		Library: convertedLibrary,
		Name:    Identifier(name),
		Member:  Identifier(member),
	}
}

func TestDeclInfo(t *testing.T) {
	valResourceness := IsValueType
	resResourceness := IsResourceType

	bitsShape := TypeShape{InlineSize: 4, Alignment: 4}
	enumInt8Shape := TypeShape{InlineSize: 1, Alignment: 1}
	enumUint64Shape := TypeShape{InlineSize: 8, Alignment: 8}
	newTypeShape := TypeShape{InlineSize: 2, Alignment: 2}
	overlayShape := TypeShape{InlineSize: 24, Alignment: 8, HasPadding: true}
	structShape := TypeShape{InlineSize: 16, Alignment: 8, Depth: 1, MaxOutOfLine: 8}
	extStructShape := TypeShape{InlineSize: 8, Alignment: 4, MaxHandles: 1}
	tableShape := TypeShape{InlineSize: 16, Alignment: 8, Depth: 2, HasFlexibleEnvelope: true}
	unionShape := TypeShape{InlineSize: 16, Alignment: 8, Depth: 1, MaxHandles: 2, HasFlexibleEnvelope: true}
	importedStructShape := TypeShape{InlineSize: 32, Alignment: 8}

	root := Root{
		Consts: []Const{
			{decl: decl{Name: "test.lib/MyConst"}},
		},
		Bits: []Bits{
			{
				layoutDecl: layoutDecl{decl: decl{Name: "test.lib/MyBits"}},
				Type:       Type{Kind: PrimitiveType, PrimitiveSubtype: Uint32, TypeShapeV2: bitsShape},
			},
		},
		Enums: []Enum{
			{
				layoutDecl: layoutDecl{decl: decl{Name: "test.lib/MyInt8Enum"}},
				Type:       Int8,
			},
			{
				layoutDecl: layoutDecl{decl: decl{Name: "test.lib/MyUint64Enum"}},
				Type:       Uint64,
			},
		},
		Resources: []Resource{
			{decl: decl{Name: "test.lib/MyResource"}},
		},
		Protocols: []Protocol{
			{decl: decl{Name: "test.lib/MyProtocol"}},
		},
		Services: []Service{
			{decl: decl{Name: "test.lib/MyService"}},
		},
		Structs: []Struct{
			{
				resourceableLayoutDecl: resourceableLayoutDecl{
					layoutDecl:   layoutDecl{decl: decl{Name: "test.lib/MyStruct"}},
					Resourceness: IsValueType,
				},
				TypeShapeV2: structShape,
			},
		},
		ExternalStructs: []Struct{
			{
				resourceableLayoutDecl: resourceableLayoutDecl{
					layoutDecl:   layoutDecl{decl: decl{Name: "other.lib/ExtStruct"}},
					Resourceness: IsResourceType,
				},
				TypeShapeV2: extStructShape,
			},
		},
		Tables: []Table{
			{
				resourceableLayoutDecl: resourceableLayoutDecl{
					layoutDecl:   layoutDecl{decl: decl{Name: "test.lib/MyTable"}},
					Resourceness: IsValueType,
				},
				TypeShapeV2: tableShape,
			},
		},
		Unions: []Union{
			{
				resourceableLayoutDecl: resourceableLayoutDecl{
					layoutDecl:   layoutDecl{decl: decl{Name: "test.lib/MyUnion"}},
					Resourceness: IsResourceType,
				},
				TypeShapeV2: unionShape,
			},
		},
		Overlays: []Overlay{
			{
				resourceableLayoutDecl: resourceableLayoutDecl{
					layoutDecl:   layoutDecl{decl: decl{Name: "test.lib/MyOverlay"}},
					Resourceness: IsValueType,
				},
				TypeShapeV2: overlayShape,
			},
		},
		Aliases: []Alias{
			{decl: decl{Name: "test.lib/MyAlias"}},
		},
		NewTypes: []NewType{
			{
				decl: decl{Name: "test.lib/MyNewType"},
				Type: Type{Kind: PrimitiveType, PrimitiveSubtype: Uint16, TypeShapeV2: newTypeShape},
			},
		},
		Libraries: []Library{
			{
				Name: "dep.lib",
				Decls: DeclInfoMap{
					"dep.lib/ImportedStruct": {
						Type:         StructDeclType,
						Resourceness: &valResourceness,
						TypeShapeV2:  &importedStructShape,
					},
				},
			},
		},
	}

	expected := DeclInfoMap{
		"test.lib/MyConst": {
			Type: ConstDeclType,
		},
		"test.lib/MyBits": {
			Type:        BitsDeclType,
			TypeShapeV2: &bitsShape,
		},
		"test.lib/MyInt8Enum": {
			Type:        EnumDeclType,
			TypeShapeV2: &enumInt8Shape,
		},
		"test.lib/MyUint64Enum": {
			Type:        EnumDeclType,
			TypeShapeV2: &enumUint64Shape,
		},
		"test.lib/MyResource": {
			Type: ResourceDeclType,
		},
		"test.lib/MyProtocol": {
			Type: ProtocolDeclType,
		},
		"test.lib/MyService": {
			Type: ServiceDeclType,
		},
		"test.lib/MyStruct": {
			Type:         StructDeclType,
			Resourceness: &valResourceness,
			TypeShapeV2:  &structShape,
		},
		"other.lib/ExtStruct": {
			Type:         StructDeclType,
			Resourceness: &resResourceness,
			TypeShapeV2:  &extStructShape,
		},
		"test.lib/MyTable": {
			Type:         TableDeclType,
			Resourceness: &valResourceness,
			TypeShapeV2:  &tableShape,
		},
		"test.lib/MyUnion": {
			Type:         UnionDeclType,
			Resourceness: &resResourceness,
			TypeShapeV2:  &unionShape,
		},
		"test.lib/MyOverlay": {
			Type:         OverlayDeclType,
			Resourceness: &valResourceness,
			TypeShapeV2:  &overlayShape,
		},
		"test.lib/MyAlias": {
			Type: AliasDeclType,
		},
		"test.lib/MyNewType": {
			Type:        NewTypeDeclType,
			TypeShapeV2: &newTypeShape,
		},
		"dep.lib/ImportedStruct": {
			Type:         StructDeclType,
			Resourceness: &valResourceness,
			TypeShapeV2:  &importedStructShape,
		},
	}

	actual := root.DeclInfo()
	if diff := cmp.Diff(expected, actual); len(diff) > 0 {
		t.Errorf("unexpected DeclInfo() diff (-want +got):\n%s", diff)
	}
}
