package parser

import (
	"reflect"
)

type Visitor func(node Node, next func() error) error

// Visit all nodes in the AST.
func Visit(node Node, visitor Visitor) error {
	if node == nil || reflect.ValueOf(node).IsZero() {
		return nil
	}
	return visitor(node, func() error {
		for _, child := range node.children() {
			err := Visit(child, visitor)
			if err != nil {
				return err
			}
		}
		return nil
	})
}

func (a *Assignment) children() []Node { return []Node{a.Value} }

func (b *Bitfile) children() []Node {
	out := make([]Node, len(b.Entries))
	for i, e := range b.Entries {
		out[i] = e
	}
	return out
}

func (t *Target) children() []Node {
	var out []Node
	out = append(out, t.Inputs)
	out = append(out, t.Outputs)
	for _, d := range t.Directives {
		out = append(out, d)
	}
	return out
}

func (t *VirtualTarget) children() []Node {
	var out []Node
	out = append(out, t.Inputs)
	for _, d := range t.Directives {
		out = append(out, d)
	}
	return out
}

func (t *Template) children() []Node {
	var out []Node
	for _, p := range t.Parameters {
		out = append(out, p)
	}
	out = append(out, t.Inputs)
	out = append(out, t.Outputs)
	for _, d := range t.Directives {
		out = append(out, d)
	}
	return out
}

func (i *Inherit) children() []Node {
	out := make([]Node, 0, len(i.Parameters))
	for _, p := range i.Parameters {
		out = append(out, p)
	}
	return out
}

func (r *RefList) children() []Node {
	if r == nil {
		return nil
	}
	out := make([]Node, len(r.Refs))
	for i, n := range r.Refs {
		out[i] = n
	}
	return out
}

func (c *Command) children() []Node { return []Node{c.Value} }

func (a *Argument) children() []Node { return []Node{a.Value} }

func (p *Parameter) children() []Node { return []Node{p.Value} }

func (d *Dir) children() []Node { return []Node{d.Target} }

func (*Block) children() []Node { return nil }

func (r *RefCommand) children() []Node { return []Node{r.Value} }

func (*String) children() []Node { return nil }

func (*Ref) children() []Node { return nil }
