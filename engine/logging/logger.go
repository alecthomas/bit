package logging

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"runtime"
	"strings"
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

// Exec a command
//
// TODO: Override the PTY width to be the terminal width minus the margin. We'd have
// to trap SIGWINCH, get our width, then update the PTY width accordingly.
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
	// Resize the PTY to exclude the margin.
	if w, h, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
		_ = pty.Setsize(p, &pty.Winsize{Rows: uint16(h), Cols: uint16(w) - 17})
	}
	defer t.Close()
	defer p.Close()
	lw := l.WriterAt(LogLevelInfo)
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
func (l *Logger) WriterAt(level LogLevel) *io.PipeWriter {
	// Based on MIT licensed Logrus https://github.com/sirupsen/logrus/blob/bdc0db8ead3853c56b7cd1ac2ba4e11b47d7da6b/writer.go#L27
	reader, writer := io.Pipe()

	go l.writerScanner(reader, level)
	runtime.SetFinalizer(writer, writerFinalizer)

	return writer
}

// There is a bit of magic here to support cursor horizontal absolute escape
// sequences. When a new position is requested, we add the width of the margin.
func (l *Logger) writerScanner(r *io.PipeReader, level LogLevel) {
	defer r.Close()

	esc := csi.NewReader(r)
	for {
		segment, err := esc.Read()
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrClosedPipe) {
			return
		} else if err != nil {
			l.Warnf("error reading CSI sequence: %s", err)
			return
		}
		if cs, ok := segment.(csi.CSI); ok {
			if cs.Final == 'G' {
				params, err := cs.IntParams()
				if err != nil || len(params) != 1 {
					segment = csi.Text(cs.String())
				} else {
					// We have a CHA sequence, so add the margin width.
					col := params[0]
					col += 18
					segment = csi.Text(fmt.Sprintf("\033[%dG", col))
				}
			} else {
				segment = csi.Text(cs.String())
			}
		}

		for _, b := range segment.(csi.Text) { //nolint:forcetypeassert
			os.Stdout.Write([]byte{b})
			if b == '\r' || b == '\n' {
				os.Stdout.Write([]byte(l.getPrefix(level)))
			}
		}
	}
}

func writerFinalizer(writer *io.PipeWriter) {
	_ = writer.Close()
}
