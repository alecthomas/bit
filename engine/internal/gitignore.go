package internal

import (
	"bufio"
	"os"
	"path"
	"path/filepath"
	"strings"
)

func LoadGitIgnore(dir string) []string {
	ignore := []string{
		"**/.*",
		"**/.*/**",
	}
	r, err := os.Open(filepath.Join(dir, ".gitignore"))
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
