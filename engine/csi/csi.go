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
	for {
		var buf []byte
		b, err := r.r.ReadByte()
		if err != nil {
			return nil, err
		}
		buf = append(buf, b)
		switch b {
		case '\033':
			b, err := r.r.ReadByte()
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

		default:
			return Text(buf), nil
		}
	}
}

// Segment is a segment of a terminal stream.
//
//sumtype:decl
type Segment interface{ segment() }

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

func (c *CSI) String() string {
	return fmt.Sprintf("\033[%s%s%c", c.Params, c.Intermediate, c.Final)
}

// Text is a text segment.
type Text []byte

func (t Text) segment() {}
