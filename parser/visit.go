package parser

type Visitor func(node Node, next func() error) error

// Visit all nodes in the AST.
func Visit(node Node, visitor Visitor) error {
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
	out := make([]Node, 0, len(t.Inputs)+len(t.Outputs)+len(t.Directives))
	for _, i := range t.Inputs {
		out = append(out, i)
	}
	for _, o := range t.Outputs {
		out = append(out, o)
	}
	for _, d := range t.Directives {
		out = append(out, d)
	}
	return out
}

func (t *VirtualTarget) children() []Node {
	out := make([]Node, 0, len(t.Inputs)+len(t.Directives))
	for _, i := range t.Inputs {
		out = append(out, i)
	}
	for _, d := range t.Directives {
		out = append(out, d)
	}
	return out
}

func (t *Template) children() []Node {
	out := make([]Node, 0, len(t.Parameters)+len(t.Inputs)+len(t.Outputs)+len(t.Directives))
	for _, p := range t.Parameters {
		out = append(out, p)
	}
	for _, i := range t.Inputs {
		out = append(out, i)
	}
	for _, o := range t.Outputs {
		out = append(out, o)
	}
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

func (c *Command) children() []Node { return []Node{c.Value} }

func (Argument) children() []Node { return nil }

func (Parameter) children() []Node { return nil }

func (d *Dir) children() []Node { return []Node{d.Target} }

func (Block) children() []Node { return nil }

func (r *RefCommand) children() []Node {
	out := make([]Node, 0, len(r.Value))
	for _, v := range r.Value {
		out = append(out, v)
	}
	return out
}

func (Var) children() []Node { return nil }

func (Cmd) children() []Node { return nil }

func (String) children() []Node { return nil }

func (Path) children() []Node { return nil }
