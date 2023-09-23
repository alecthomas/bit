// Package csi provides a parser for CSI (Control Sequence Introducer) escape
// sequences.
package csi

import (
	"bufio"
	"fmt"
	"io"
	"strconv"
	"strings"
)

type Reader struct {
	r *bufio.Reader
}

// NewReader creates a new reader of CSI sequences.
func NewReader(r io.Reader) *Reader {
	return &Reader{r: bufio.NewReader(r)}
}

func (r *Reader) Read() (Segment, error) {
	var buf []byte
	for {
		b, err := r.r.ReadByte()
		if err != nil {
			if buf != nil {
				return Text(buf), nil
			}
			return nil, err
		}

		// Not an escape sequence, accumulate.
		if b != '\033' {
			buf = append(buf, b)
			continue
		}

		// Flush any buffered text.
		if b == '\033' && len(buf) > 0 {
			_ = r.r.UnreadByte()
			return Text(buf), nil
		}

		buf = append(buf, b)

		// Escape sequence.
		b, err = r.r.ReadByte()
		if err != nil {
			return Text(buf), nil
		}
		buf = append(buf, b)
		c := CSI{}
		for {
			// Aborted escape sequence.
			b, err := r.r.ReadByte()
			if err != nil {
				return Text(buf), nil
			}
			buf = append(buf, b)
			switch {
			case b >= 0x30 && b <= 0x3f:
				c.Params = append(c.Params, b)

			case b >= 0x20 && b <= 0x2f:
				c.Intermediate = append(c.Intermediate, b)

			case b >= 0x40 && b <= 0x7e:
				c.Final = b
				return c, nil

			default:
				return Text(buf), nil
			}
		}
	}
}

// Segment of a terminal stream, either [CSI] or [Text].
//
//sumtype:decl
type Segment interface {
	segment()
	String() string
}

var _ Segment = CSI{}

// CSI represents an escape sequence.
type CSI struct {
	Params       []byte
	Intermediate []byte
	Final        byte
}

func (c CSI) segment() {}

// IntParams returns the ";"-separated CSI parameters as a slice of integers.
func (c CSI) IntParams() ([]int, error) {
	var out []int
	for _, p := range strings.Split(string(c.Params), ";") {
		i, err := strconv.Atoi(p)
		if err != nil {
			return nil, err
		}
		out = append(out, i)
	}
	return out, nil
}

func (c CSI) String() string {
	return fmt.Sprintf("\033[%s%s%c", c.Params, c.Intermediate, c.Final)
}

var _ Segment = Text{}

// Text is a text segment.
type Text []byte

func (t Text) segment()       {}
func (t Text) String() string { return string(t) }
