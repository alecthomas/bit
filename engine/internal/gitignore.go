package internal

import (
	"bufio"
	"io/fs"
	"path"
	"strings"
)

func LoadGitIgnore(root fs.FS, dir string) []string {
	ignore := []string{
		"**/.*",
		"**/.*/**",
	}
	r, err := root.Open(path.Join(dir, ".gitignore"))
	if err != nil {
		return nil
	}
	lr := bufio.NewScanner(r)
	for lr.Scan() {
		line := lr.Text()
		line = strings.TrimSpace(line)
		if line == "" || line[0] == '#' || line[0] == '!' { // We don't support negation.
			continue
		}
		if strings.HasSuffix(line, "/") {
			line = path.Join("**", line, "**/*")
		} else if !strings.ContainsRune(line, '/') {
			line = path.Join("**", line)
		}
		ignore = append(ignore, line)
	}
	return ignore
}
