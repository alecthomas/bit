package main

//go:generate enumer -type=Enum,Kinds -json -text
type Enum int

const (
	EnumA Enum = iota
	EnumB
)

type Kinds int

const (
	EnumOne Kinds = iota
	EnumTwo
)

func main() {
}
