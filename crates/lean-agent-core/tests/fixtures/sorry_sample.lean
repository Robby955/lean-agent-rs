import Init

namespace Sample

/-- A theorem left open. -/
theorem foo (n : Nat) : n = n := by
  sorry

def bar : Nat := by
  admit

theorem baz : True := by
  trivial

end Sample
