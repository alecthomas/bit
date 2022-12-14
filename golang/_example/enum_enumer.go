// Code generated by "enumer -type=Enum,Kinds -json -text"; DO NOT EDIT.

package main

import (
	"encoding/json"
	"fmt"
	"strings"
)

const _EnumName = "EnumAEnumB"

var _EnumIndex = [...]uint8{0, 5, 10}

const _EnumLowerName = "enumaenumb"

func (i Enum) String() string {
	if i < 0 || i >= Enum(len(_EnumIndex)-1) {
		return fmt.Sprintf("Enum(%d)", i)
	}
	return _EnumName[_EnumIndex[i]:_EnumIndex[i+1]]
}

// An "invalid array index" compiler error signifies that the constant values have changed.
// Re-run the stringer command to generate them again.
func _EnumNoOp() {
	var x [1]struct{}
	_ = x[EnumA-(0)]
	_ = x[EnumB-(1)]
}

var _EnumValues = []Enum{EnumA, EnumB}

var _EnumNameToValueMap = map[string]Enum{
	_EnumName[0:5]:       EnumA,
	_EnumLowerName[0:5]:  EnumA,
	_EnumName[5:10]:      EnumB,
	_EnumLowerName[5:10]: EnumB,
}

var _EnumNames = []string{
	_EnumName[0:5],
	_EnumName[5:10],
}

// EnumString retrieves an enum value from the enum constants string name.
// Throws an error if the param is not part of the enum.
func EnumString(s string) (Enum, error) {
	if val, ok := _EnumNameToValueMap[s]; ok {
		return val, nil
	}

	if val, ok := _EnumNameToValueMap[strings.ToLower(s)]; ok {
		return val, nil
	}
	return 0, fmt.Errorf("%s does not belong to Enum values", s)
}

// EnumValues returns all values of the enum
func EnumValues() []Enum {
	return _EnumValues
}

// EnumStrings returns a slice of all String values of the enum
func EnumStrings() []string {
	strs := make([]string, len(_EnumNames))
	copy(strs, _EnumNames)
	return strs
}

// IsAEnum returns "true" if the value is listed in the enum definition. "false" otherwise
func (i Enum) IsAEnum() bool {
	for _, v := range _EnumValues {
		if i == v {
			return true
		}
	}
	return false
}

// MarshalJSON implements the json.Marshaler interface for Enum
func (i Enum) MarshalJSON() ([]byte, error) {
	return json.Marshal(i.String())
}

// UnmarshalJSON implements the json.Unmarshaler interface for Enum
func (i *Enum) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err != nil {
		return fmt.Errorf("Enum should be a string, got %s", data)
	}

	var err error
	*i, err = EnumString(s)
	return err
}

// MarshalText implements the encoding.TextMarshaler interface for Enum
func (i Enum) MarshalText() ([]byte, error) {
	return []byte(i.String()), nil
}

// UnmarshalText implements the encoding.TextUnmarshaler interface for Enum
func (i *Enum) UnmarshalText(text []byte) error {
	var err error
	*i, err = EnumString(string(text))
	return err
}

const _KindsName = "EnumOneEnumTwo"

var _KindsIndex = [...]uint8{0, 7, 14}

const _KindsLowerName = "enumoneenumtwo"

func (i Kinds) String() string {
	if i < 0 || i >= Kinds(len(_KindsIndex)-1) {
		return fmt.Sprintf("Kinds(%d)", i)
	}
	return _KindsName[_KindsIndex[i]:_KindsIndex[i+1]]
}

// An "invalid array index" compiler error signifies that the constant values have changed.
// Re-run the stringer command to generate them again.
func _KindsNoOp() {
	var x [1]struct{}
	_ = x[EnumOne-(0)]
	_ = x[EnumTwo-(1)]
}

var _KindsValues = []Kinds{EnumOne, EnumTwo}

var _KindsNameToValueMap = map[string]Kinds{
	_KindsName[0:7]:       EnumOne,
	_KindsLowerName[0:7]:  EnumOne,
	_KindsName[7:14]:      EnumTwo,
	_KindsLowerName[7:14]: EnumTwo,
}

var _KindsNames = []string{
	_KindsName[0:7],
	_KindsName[7:14],
}

// KindsString retrieves an enum value from the enum constants string name.
// Throws an error if the param is not part of the enum.
func KindsString(s string) (Kinds, error) {
	if val, ok := _KindsNameToValueMap[s]; ok {
		return val, nil
	}

	if val, ok := _KindsNameToValueMap[strings.ToLower(s)]; ok {
		return val, nil
	}
	return 0, fmt.Errorf("%s does not belong to Kinds values", s)
}

// KindsValues returns all values of the enum
func KindsValues() []Kinds {
	return _KindsValues
}

// KindsStrings returns a slice of all String values of the enum
func KindsStrings() []string {
	strs := make([]string, len(_KindsNames))
	copy(strs, _KindsNames)
	return strs
}

// IsAKinds returns "true" if the value is listed in the enum definition. "false" otherwise
func (i Kinds) IsAKinds() bool {
	for _, v := range _KindsValues {
		if i == v {
			return true
		}
	}
	return false
}

// MarshalJSON implements the json.Marshaler interface for Kinds
func (i Kinds) MarshalJSON() ([]byte, error) {
	return json.Marshal(i.String())
}

// UnmarshalJSON implements the json.Unmarshaler interface for Kinds
func (i *Kinds) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err != nil {
		return fmt.Errorf("Kinds should be a string, got %s", data)
	}

	var err error
	*i, err = KindsString(s)
	return err
}

// MarshalText implements the encoding.TextMarshaler interface for Kinds
func (i Kinds) MarshalText() ([]byte, error) {
	return []byte(i.String()), nil
}

// UnmarshalText implements the encoding.TextUnmarshaler interface for Kinds
func (i *Kinds) UnmarshalText(text []byte) error {
	var err error
	*i, err = KindsString(string(text))
	return err
}
