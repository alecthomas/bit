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
		Text{62, 62, 62, 34, 92, 48, 51, 51, 91, 63, 34, 62, 62, 62, 32},
		CSI{Params: []byte("?25"), Final: 108}, CSI{Params: []byte("32"), Final: 109},
		Text{226, 160, 139}, CSI{Params: []byte("39"), Final: 109}, Text{32},
		CSI{Params: []byte("90"), Final: 109}, CSI{Params: []byte("1"), Final: 109},
		Text{66, 117, 105, 108, 100, 105, 110, 103, 32, 105, 110, 100, 101, 120, 46, 104, 116, 109, 108, 46, 46, 46},
		CSI{Params: []byte("22"), Final: 109}, CSI{Params: []byte("39"), Final: 109}, Text{32, 62, 62, 62, 34, 10, 34, 62, 62, 62, 32},
		CSI{Params: []byte("2"), Final: 75}, CSI{Params: []byte("19"), Final: 71},
		CSI{Params: []byte("1"), Final: 65}, CSI{Params: []byte("2"), Final: 75},
		CSI{Params: []byte("19"), Final: 71}, CSI{Params: []byte("32"), Final: 109},
		Text{62, 62, 62, 34, 226, 34, 62, 62, 62, 32, 226, 160, 153}, CSI{Params: []byte("39"), Final: 109},
		Text{32}, CSI{Params: []byte("90"), Final: 109}, CSI{Params: []byte("1"), Final: 109},
		Text{66, 117, 105, 108, 100, 105, 110, 103, 32, 105, 110, 100, 101, 120, 46, 104, 116, 109, 108, 46, 46, 46},
		CSI{Params: []byte("22"), Final: 109}, CSI{Params: []byte("39"), Final: 109},
		Text{32, 62, 62, 62, 34, 10, 34, 62, 62, 62},
	}
	assert.Equal(t, expected, segments, repr.String(segments))
}
