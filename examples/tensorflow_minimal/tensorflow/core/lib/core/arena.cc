#include "arena.h"
#include <cstdlib>

namespace tensorflow {
namespace core {

void* Arena::Allocate(std::size_t bytes) {
  return malloc(bytes);
}

}  // namespace core
}  // namespace tensorflow
