#include "arena.h"
#include <cstdlib>

namespace rush_test {

void* Arena::Allocate(size_t bytes) {
  return malloc(bytes);
}

}  // namespace rush_test
