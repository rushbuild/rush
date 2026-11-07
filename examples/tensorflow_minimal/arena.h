#ifndef ARENA_H
#define ARENA_H

#include <cstddef>

namespace rush_test {

class Arena {
 public:
  Arena() {}
  void* Allocate(size_t bytes);
};

}  // namespace rush_test

#endif  // ARENA_H
