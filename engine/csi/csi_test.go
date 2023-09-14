package csi

import (
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/alecthomas/repr"
)

func TestCSI(t *testing.T) {
	input := strings.NewReader(
		"\033[0J\033[0J>>>\"\\033[?\">>> \033[?25l\033[32m⠋\033[39m \033[90m\033[1mBuilding index.html...\033[22m\033[39m >>>\"\n\">>> " +
			"\033[2K\033[19G\033[1A\033[2K\033[19G\033[32m>>>\"\xe2\">>> ⠙\033[39m \033[90m\033[1mBuilding index.html...\033[22m\033[39m >>>\"\n\">>>")
	reader := NewReader(input)
	var segments []Segment
	for {
		segment, err := reader.Read()
		if errors.Is(err, io.EOF) {
			break
		}
		assert.NoError(t, err)
		segments = append(segments, segment)
	}
	expected := []Segment{
		CSI{Params: []byte("0"), Final: 74}, CSI{Params: []byte("0"), Final: 74},
		Text{62}, Text{62}, Text{62}, Text{34}, Text{92}, Text{48}, Text{51},
		Text{51}, Text{91}, Text{63}, Text{34}, Text{62}, Text{62}, Text{62},
		Text{32}, CSI{Params: []byte("?25"), Final: 108}, CSI{Params: []byte("32"), Final: 109},
		Text{226}, Text{160}, Text{139}, CSI{Params: []byte("39"), Final: 109},
		Text{32}, CSI{Params: []byte("90"), Final: 109}, CSI{Params: []byte("1"), Final: 109},
		Text{66}, Text{117}, Text{105}, Text{108}, Text{100}, Text{105}, Text{110},
		Text{103}, Text{32}, Text{105}, Text{110}, Text{100}, Text{101}, Text{120},
		Text{46}, Text{104}, Text{116}, Text{109}, Text{108}, Text{46}, Text{46},
		Text{46}, CSI{Params: []byte("22"), Final: 109}, CSI{Params: []byte("39"), Final: 109},
		Text{32}, Text{62}, Text{62}, Text{62}, Text{34}, Text{10}, Text{34},
		Text{62}, Text{62}, Text{62}, Text{32}, CSI{Params: []byte("2"), Final: 75},
		CSI{Params: []byte("19"), Final: 71}, CSI{Params: []byte("1"), Final: 65},
		CSI{Params: []byte("2"), Final: 75}, CSI{Params: []byte("19"), Final: 71},
		CSI{Params: []byte("32"), Final: 109}, Text{62}, Text{62}, Text{62}, Text{34},
		Text{226}, Text{34}, Text{62}, Text{62}, Text{62}, Text{32}, Text{226},
		Text{160}, Text{153}, CSI{Params: []byte("39"), Final: 109}, Text{32},
		CSI{Params: []byte("90"), Final: 109}, CSI{Params: []byte("1"), Final: 109},
		Text{66}, Text{117}, Text{105}, Text{108}, Text{100}, Text{105}, Text{110},
		Text{103}, Text{32}, Text{105}, Text{110}, Text{100}, Text{101}, Text{120},
		Text{46}, Text{104}, Text{116}, Text{109}, Text{108}, Text{46}, Text{46},
		Text{46}, CSI{Params: []byte("22"), Final: 109},
		CSI{Params: []byte("39"), Final: 109}, Text{32}, Text{62}, Text{62}, Text{62},
		Text{34}, Text{10}, Text{34}, Text{62}, Text{62}, Text{62},
	}
	assert.Equal(t, expected, segments, repr.String(segments))
}
