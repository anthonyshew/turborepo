package auth

import "testing"

func TestGreeting(t *testing.T) {
  if Greeting() != "hello from Go" { t.Fatal("unexpected greeting") }
}
