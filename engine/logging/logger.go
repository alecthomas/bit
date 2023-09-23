package logging

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"sync"
	"syscall"

	"github.com/alecthomas/bit/engine/csi"
	"github.com/creack/pty"
	"github.com/kballard/go-shellquote"
	"github.com/mattn/go-isatty"
	"golang.org/x/term"
)

type LogConfig struct {
	Level LogLevel `help:"Log level (${enum})." enum:"trace,debug,info,notice,warn,error" default:"info"`
	Debug bool     `help:"Enable debug mode." xor:"trace"`
	Trace bool     `help:"Enable trace mode." xor:"trace"`
}

type LogLevel int

const (
	LogLevelTrace LogLevel = iota
	LogLevelDebug
	LogLevelInfo
	LogLevelNotice
	LogLevelWarn
	LogLevelError
)

func (l LogLevel) String() string {
	switch l {
	case LogLevelTrace:
		return "trace"
	case LogLevelDebug:
		return "debug"
	case LogLevelInfo:
		return "info"
	case LogLevelNotice:
		return "notice"
	case LogLevelWarn:
		return "warn"
	case LogLevelError:
		return "error"
	default:
		return fmt.Sprintf("LogLevel(%d)", l)
	}
}

func (l *LogLevel) UnmarshalText(text []byte) error {
	switch string(text) {
	case "trace":
		*l = LogLevelTrace
	case "debug":
		*l = LogLevelDebug
	case "info":
		*l = LogLevelInfo
	case "notice":
		*l = LogLevelNotice
	case "warn":
		*l = LogLevelWarn
	case "error":
		*l = LogLevelError
	default:
		return fmt.Errorf("invalid log level %q", text)
	}
	return nil
}

type Logger struct {
	level LogLevel
	scope string
}

func NewLogger(config LogConfig) *Logger {
	level := config.Level
	if config.Trace {
		level = LogLevelTrace
	} else if config.Debug {
		level = LogLevelDebug
	}
	return &Logger{level: level}
}

// Scope returns a new logger with the given scope.
func (l *Logger) Scope(scope string) *Logger {
	if len(scope) > 16 {
		scope = "…" + scope[len(scope)-15:]
	}
	scope = fmt.Sprintf("%-16s", scope)
	scope = strings.ReplaceAll(scope, "%", "%%")
	return &Logger{scope: scope, level: l.level}
}

var ansiTable = func() map[LogLevel]string {
	if !isatty.IsTerminal(os.Stdout.Fd()) {
		return map[LogLevel]string{}
	}
	return map[LogLevel]string{
		LogLevelTrace:  "\033[90m",
		LogLevelDebug:  "\033[34m",
		LogLevelInfo:   "",
		LogLevelNotice: "\033[32m",
		LogLevelWarn:   "\033[33m",
		LogLevelError:  "\033[31m",
	}
}()

func (l *Logger) writePrefix(level LogLevel) {
	if l.level > level {
		return
	}
	fmt.Print(l.getPrefix(level))
}

func (l *Logger) getPrefix(level LogLevel) string {
	if l.level > level {
		return ""
	}
	prefix := ansiTable[level]
	if l.scope != "" {
		prefix = targetColour(l.scope) + l.scope + "\033[0m" + "| " + prefix
	}
	return prefix
}

func (l *Logger) logf(level LogLevel, format string, args ...interface{}) {
	if l.level > level {
		return
	}
	l.writePrefix(level)
	fmt.Printf(format+"\033[0m\n", args...)
}

func (l *Logger) Noticef(format string, args ...interface{}) {
	l.logf(LogLevelNotice, format, args...)
}

func (l *Logger) Infof(format string, args ...interface{}) {
	l.logf(LogLevelInfo, format, args...)
}

func (l *Logger) Debugf(format string, args ...interface{}) {
	l.logf(LogLevelDebug, format, args...)
}

func (l *Logger) Tracef(format string, args ...interface{}) {
	l.logf(LogLevelTrace, format, args...)
}

func (l *Logger) Warnf(format string, args ...interface{}) {
	l.logf(LogLevelWarn, format, args...)
}

func (l *Logger) Errorf(format string, args ...interface{}) {
	l.logf(LogLevelError, format, args...)
}

// Exec a command.
func (l *Logger) Exec(dir, command string) error {
	if dir == "" || dir == "." {
		dir = "."
	} else {
		l.Noticef("$ cd %s", shellquote.Join(dir))
	}
	lines := strings.Split(command, "\n")
	for i, line := range lines {
		if i == 0 {
			l.Noticef("$ %s", line)
		} else {
			l.Noticef("  %s", line)
		}
	}

	p, t, err := pty.Open()
	if err != nil {
		return err
	}

	winch := make(chan os.Signal, 1)
	signal.Notify(winch, syscall.SIGWINCH)
	defer signal.Stop(winch)
	go func() {
		for range winch {
			if w, h, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
				_ = pty.Setsize(p, &pty.Winsize{Rows: uint16(h), Cols: uint16(w) - 17})
			}
		}
	}()
	// Resize the PTY to exclude the margin.
	if w, h, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
		_ = pty.Setsize(p, &pty.Winsize{Rows: uint16(h), Cols: uint16(w) - 17})
	}
	defer t.Close()
	defer p.Close()
	lw := l.WriterAt(LogLevelInfo)
	defer lw.Close()
	cmd := exec.Command("/bin/sh", "-c", command)
	cmd.Dir = dir
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid:  true,
		Setctty: true,
	}
	cmd.Stdout = t
	cmd.Stderr = t
	cmd.Stdin = t
	if err := cmd.Start(); err != nil {
		return err
	}
	go io.Copy(lw, p) //nolint:errcheck
	return cmd.Wait()
}

// WriterAt returns a writer that logs each line at the given level.
func (l *Logger) WriterAt(level LogLevel) io.WriteCloser {
	// Based on MIT licensed Logrus https://github.com/sirupsen/logrus/blob/bdc0db8ead3853c56b7cd1ac2ba4e11b47d7da6b/writer.go#L27
	reader, writer := io.Pipe()

	wg := &sync.WaitGroup{}
	wg.Add(1)
	go l.writerScanner(wg, reader, level)
	return &pipeWait{r: reader, PipeWriter: writer, wg: wg}
}

type pipeWait struct {
	r *io.PipeReader
	*io.PipeWriter
	wg *sync.WaitGroup
}

func (p pipeWait) Close() error {
	err := p.PipeWriter.Close()
	p.wg.Wait()
	return err
}

// There is a bit of magic here to support cursor horizontal absolute escape
// sequences. When a new position is requested, we add the width of the margin.
func (l *Logger) writerScanner(wg *sync.WaitGroup, r *io.PipeReader, level LogLevel) {
	defer r.Close()
	defer wg.Done()

	esc := csi.NewReader(r)
	drawPrefix := true
	var newline []byte
	for {
		segment, err := esc.Read()
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrClosedPipe) {
			os.Stdout.Write(newline)
			return
		} else if err != nil {
			l.Warnf("error reading CSI sequence: %s", err)
			return
		}

		// If we have a CSI sequence, possibly intercept it to cater for the margin.
		// But in all cases we want to transform it to Text.
		if cs, ok := segment.(csi.CSI); ok {
			// All the cases we intercept are single parameter sequences.
			params, err := cs.IntParams()
			if err != nil || len(params) != 1 {
				segment = csi.Text(cs.String())
			} else {
				switch cs.Final {
				case 'G': // G is cursor horizontal absolute.
					// We have a CHA sequence, so add the margin width.
					col := params[0] + 18
					segment = csi.Text(fmt.Sprintf("\033[%dG", col))

				case 'K': // K is erase in line. We want to intercept 1 (clear to start of line) and 2 (clear entire line).
					if params[0] == 1 || params[0] == 2 {
						// Save the cursor position.
						text := []byte("\033[s")
						// Apply the CSI.
						text = append(text, cs.String()...)
						// Move to the start of the line.
						text = append(text, "\033[1G"...)
						// Write the prefix.
						text = append(text, l.getPrefix(level)...)
						// Restore the cursor position.
						text = append(text, "\033[u"...)

						segment = csi.Text(text)
					} else {
						segment = csi.Text(cs.String())
					}
				default:
					segment = csi.Text(cs.String())
				}
			}
		}

		for _, b := range segment.(csi.Text) { //nolint:forcetypeassert
			if b == '\r' || b == '\n' {
				newline = append(newline, b)
				continue
			}
			if drawPrefix {
				os.Stdout.Write([]byte(l.getPrefix(level)))
				drawPrefix = false
			}
			for _, nl := range newline {
				os.Stdout.Write([]byte{nl})
				if nl == '\n' {
					os.Stdout.Write([]byte(l.getPrefix(level)))
				}
			}
			newline = nil
			os.Stdout.Write([]byte{b})
		}
	}
}
