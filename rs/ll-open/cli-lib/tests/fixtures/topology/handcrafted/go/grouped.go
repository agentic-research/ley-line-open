package grouped

// Multi-line Go import block (the block-mode parser at extract_specifiers
// is otherwise unexercised by the handcrafted fixture).
import (
	"fmt"
	"github.com/foo/bar"
	"example.com/quux"
)

func Group() {
	_ = bar.Hello()
	_ = quux.Q()
	fmt.Println("ok")
}
